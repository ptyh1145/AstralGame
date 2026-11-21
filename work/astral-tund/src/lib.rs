//! 把联机核心暴露成 C ABI，给原生 UI（Objective-C）调用。
//!
//! 设计要点：
//!   * `astral_start` **立即返回**，真正的连接过程在后台 tokio runtime 线程里跑；
//!     UI 通过轮询 `astral_state` / `astral_log` 显示进度。
//!   * 状态和日志都放在全局静态里，返回 `*const c_char`（内存常驻，UI 不用释放）。
//!   * 只暴露 4 个函数，避免把 Rust 类型泄漏给 ObjC。

use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

/// RAII: 持有 utun fd，任何提前返回（configure/create/attach 失败）时自动
/// close，避免反复点连接把 utun0..7 全部泄漏占满（表现为 EBUSY）。
struct UtunFd(RawFd);
impl Drop for UtunFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {libc::close(self.0)};
            self.0 = -1;
        }
    }
}

pub mod bridge;
pub mod cli;
pub mod utun;

// ---------------------------------------------------------------- 状态

pub const STATE_IDLE: u8 = 0;
pub const STATE_STARTING: u8 = 1;
pub const STATE_RUNNING: u8 = 2;
pub const STATE_ERROR: u8 = 3;

static STATE: AtomicU8 = AtomicU8::new(STATE_IDLE);

/// 昵称（EasyTier 的 hostname）：显示在同一虚拟网的成员列表里。
/// GUI 通过 astral_set_hostname 设置；未设置时用默认值。
static HOSTNAME: OnceLock<Mutex<String>> = OnceLock::new();

fn hostname_store() -> &'static Mutex<String> {
    HOSTNAME.get_or_init(|| Mutex::new("AstralGame".to_string()))
}

/// 设置昵称（对应官方 App 的「完善资料」里的昵称）。
#[no_mangle]
pub extern "C" fn astral_set_hostname(name: *const c_char) {
    if let Ok(v) = cstr_arg(name, "hostname") {
        if !v.trim().is_empty() {
            *hostname_store().lock().unwrap() = v.trim().to_string();
            push_log(&format!("昵称已设置: {}", v.trim()));
        }
    }
}

static LOG: OnceLock<Mutex<String>> = OnceLock::new();
/// 缓存给 C 用的 CString，避免每次调用都分配（也保证指针生命周期）
static LOG_C: OnceLock<Mutex<CString>> = OnceLock::new();
static STATE_TEXT: OnceLock<Mutex<CString>> = OnceLock::new();

fn log_store() -> &'static Mutex<String> {
    LOG.get_or_init(|| Mutex::new(String::new()))
}

fn log_c_store() -> &'static Mutex<CString> {
    LOG_C.get_or_init(|| Mutex::new(CString::new("").unwrap()))
}

fn state_text_store() -> &'static Mutex<CString> {
    STATE_TEXT.get_or_init(|| Mutex::new(CString::new("idle").unwrap()))
}

/// 打开 EasyTier 内部的 tracing 日志。
///
/// 为什么必须有：EasyTier 全程用 `tracing` 输出"连上 peer / 分配到 IP /
/// tun 设备就绪 / 加入网络"这些关键事件，而**没有 subscriber 时它们会被
/// 静默丢弃**——之前排查"安卓看不到 iOS"时日志里一片空白，就是因为只看到
/// 我们自己 println 的那几行，协议层在干什么完全不可见。
///
/// 幂等：多次调用只生效一次。输出走 stderr，daemon 模式下由 launchd
/// 重定向进 /var/log/astral-tund.log。
pub fn init_tracing() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // 数据面诊断期临时开到 DEBUG：EasyTier 的 tun 层每收/发一个包都有
        // debug 日志（convert zc packet to tun packet / opened shared
        // virtual nic mobile tun）。ping 不通时它能把"包没进 tun"还是
        // "包进了但转发失败"直接区分开。问题定位后调回 INFO（刷屏）。
        let installed = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_target(true)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init()
            .is_ok();
        if installed {
            push_log("tracing 已启用（DEBUG 级）：EasyTier 内部日志（peer/IP/tun/包流）会一并输出");
        } else {
            push_log("tracing 初始化失败（可能已有全局 subscriber），继续运行");
        }
    });
}

/// 追加一行日志（同时打印到 stderr，方便 Xcode / Console 里看）
pub fn push_log(line: &str) {
    eprintln!("[astral] {line}");
    if let Ok(mut s) = log_store().lock() {
        if s.len() > 200_000 {
            // 防止无限增长
            let cut = s.len() - 100_000;
            s.drain(..cut);
        }
        s.push_str(line);
        s.push('\n');
    }
    if let Ok(mut c) = log_c_store().lock() {
        if let Ok(new) = CString::new(log_store().lock().unwrap().as_str()) {
            *c = new;
        }
    }
}

fn set_state(st: u8, text: &str) {
    STATE.store(st, Ordering::SeqCst);
    if let Ok(mut c) = state_text_store().lock() {
        if let Ok(new) = CString::new(text) {
            *c = new;
        }
    }
    push_log(&format!("[state] {text}"));
}

// ---------------------------------------------------------------- 会话

struct Session {
    instance_id: uuid::Uuid,
    runtime: Option<tokio::runtime::Runtime>,
    /// 我们自己开的 utun fd。EasyTier 以 close_fd_on_drop(false) 接管，
    /// 删除实例时**不会**关它，所以必须由 astral_stop 负责 close。
    tun_fd: RawFd,
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

/// 断开请求：让停在 park() 的工作线程退出（光靠 park 会永远卡住）。
static STOP_FLAG: AtomicU8 = AtomicU8::new(0);

/// 「正在断开」标志：astral_stop 是异步清理，期间 astral_start 必须拒绝启动，
/// 否则旧实例还没删完新实例就入网——房间里出现两个节点，甚至误关新会话的 fd。
static STOPPING: AtomicU8 = AtomicU8::new(0);

/// 工作线程句柄，astral_stop 用 `thread().unpark()` 唤醒它。
static WORKER: OnceLock<Mutex<Option<std::thread::JoinHandle<()>>>> = OnceLock::new();

fn worker_store() -> &'static Mutex<Option<std::thread::JoinHandle<()>>> {
    WORKER.get_or_init(|| Mutex::new(None))
}

fn stop_requested() -> bool {
    STOP_FLAG.load(Ordering::SeqCst) != 0
}

/// 进入停止流程（daemon 的 STOP 与 astral_stop 共用的第一步）
pub fn begin_stop() {
    STOP_FLAG.store(1, Ordering::SeqCst);
    STOPPING.store(1, Ordering::SeqCst);
    if let Ok(g) = worker_store().lock() {
        if let Some(h) = g.as_ref() {
            h.thread().unpark();
        }
    }
}

/// 停止流程收尾：关 fd、复位状态与标志
pub fn finish_stop() {
    let fd = session_store()
        .lock()
        .ok()
        .and_then(|mut g| g.take().map(|s| s.tun_fd));
    if let Some(fd) = fd {
        if fd >= 0 {
            unsafe { libc::close(fd) };
            push_log(&format!("已关闭 utun fd={fd}"));
        }
    }
    STOPPING.store(0, Ordering::SeqCst);
    set_state(STATE_IDLE, "已停止");
    push_log("已断开");
}

/// daemon 断开时取走会话里的 fd（close_instance 之后调用）
pub fn take_session_fd() -> Option<RawFd> {
    session_store()
        .lock()
        .ok()
        .and_then(|mut g| g.take().map(|s| s.tun_fd))
}

pub fn is_stopping() -> bool {
    STOPPING.load(Ordering::SeqCst) != 0
}

fn session_store() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// 当前运行中的实例 id（未运行时为 None）。
///
/// daemon 的 `PEERS` 命令靠它去 `bridge::members()` 查房间成员。
pub fn current_instance_id() -> Option<uuid::Uuid> {
    session_store().lock().ok().and_then(|s| s.as_ref().map(|x| x.instance_id))
}

fn cstr_arg(p: *const c_char, name: &str) -> Result<String> {
    if p.is_null() {
        anyhow::bail!("{name} 为空指针");
    }
    let s = unsafe { CStr::from_ptr(p) }
        .to_str()
        .with_context(|| format!("{name} 不是合法 UTF-8"))?;
    Ok(s.to_string())
}

/// 启动联机。参数同 CLI 的 `--name/--secret/--ipv4/--peer`。
///
/// 返回 0 表示已开始（异步），非 0 表示参数/环境错误（细节见 astral_log）。
#[no_mangle]
pub extern "C" fn astral_start(
    room: *const c_char,
    secret: *const c_char,
    ipv4: *const c_char,
    peer: *const c_char,
) -> i32 {
    init_tracing();
    let (room, secret, ipv4, peer) =
        match (cstr_arg(room, "room"), cstr_arg(secret, "secret"),
               cstr_arg(ipv4, "ipv4"), cstr_arg(peer, "peer")) {
            (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
            (a, b, c, d) => {
                let e = a.err().or(b.err()).or(c.err()).or(d.err()).unwrap();
                set_state(STATE_ERROR, "参数错误");
                push_log(&format!("参数错误: {e}"));
                return -1;
            }
        };

    if is_stopping() {
        // 上一轮断开的异步清理还没做完就来了新连接请求：
        // 放行会造成"旧实例未删、新实例入网"的双节点，甚至误关新 fd。
        push_log("正在断开中，拒绝启动；请稍候 1-2 秒再连接");
        return -3;
    }
    if STATE.load(Ordering::SeqCst) == STATE_RUNNING || STATE.load(Ordering::SeqCst) == STATE_STARTING {
        push_log("已经在运行/启动中，忽略重复请求");
        return -2;
    }

    // 新一轮连接：清掉上一轮的停止标志
    STOP_FLAG.store(0, Ordering::SeqCst);
    set_state(STATE_STARTING, "启动中");

    // 后台线程跑 tokio runtime，UI 线程不阻塞。
    // 用 current_thread 而不是 multi_thread：32 位 armv7s 用
    // has-thread-local=false 编出（TLS 用 pthread_key 模拟），多线程 runtime
    // 的 worker 调度依赖 TLS，在 iOS 6/8 老 dyld 上会挂死在 build()——
    // 症状就是状态永远停在"启动中"、连"打开 utun ..."都打不出来。
    // EasyTier 是单实例网络任务，current_thread 足够，且工程下方 astral_stop
    // 已验证该 runtime 类型在此环境可用。
    let handle = std::thread::spawn(move || {
        push_log("后台线程: 创建 tokio runtime ...");
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                set_state(STATE_ERROR, "创建 runtime 失败");
                push_log(&format!("创建 tokio runtime 失败: {e}"));
                return;
            }
        };
        push_log("后台线程: runtime 就绪，进入 bring_up ...");

        let result: Result<(uuid::Uuid, RawFd)> =
            rt.block_on(async { bring_up(&room, &secret, &ipv4, &peer).await });

        match result {
            Ok((id, fd)) => {
                // 启动过程中用户就点了断开：直接清理，不要先报"已连接"再挂着
                if stop_requested() {
                    push_log("启动期间收到断开请求，直接关闭实例");
                    if let Err(e) = rt.block_on(bridge::close_instance(id)) {
                        push_log(&format!("关闭实例失败: {e}"));
                    }
                    if fd >= 0 {
                        unsafe { libc::close(fd) };
                    }
                    push_log("已清理，未进入连接状态");
                    set_state(STATE_IDLE, "已停止");
                    return;
                }

                push_log(&format!("实例已启动: {id}"));
                *session_store().lock().unwrap() = Some(Session {
                    instance_id: id,
                    runtime: None, // runtime 由本线程持有，见下面的 park
                    tun_fd: fd,
                });
                set_state(STATE_RUNNING, "已连接");

                // 停在这里等 astral_stop。每轮都检查停止标志：unpark 之后能立刻
                // 退出，不会像原来那样 park 在死循环里、连带 runtime 一起漏掉。
                while !stop_requested() {
                    std::thread::park();
                }
                push_log("工作线程收到断开信号，退出");
            }
            Err(e) => {
                set_state(STATE_ERROR, "连接失败");
                push_log(&format!("启动失败: {e:#}"));
            }
        }
        drop(rt);
    });
    *worker_store().lock().unwrap() = Some(handle);

    0
}

/// 真正建立连接：开 utun → 配地址 → 建 EasyTier 实例 → 注入 fd
async fn bring_up(room: &str, secret: &str, ipv4: &str, peer: &str) -> Result<(uuid::Uuid, RawFd)> {
    let host = hostname_store().lock().unwrap().clone();
    let dhcp = DHCP_ON.load(Ordering::SeqCst) != 0;
    let quic = QUIC_ON.load(Ordering::SeqCst) != 0;
    let toml = cli::build_toml(&host, room, secret, ipv4, &[peer.to_string()], dhcp, quic);

    let (ip, prefix) = match ipv4.split_once('/') {
        Some((ip, p)) => (ip.to_string(), p.parse::<u8>().unwrap_or(24)),
        None => (ipv4.to_string(), 24u8),
    };

    push_log("打开 utun ...");
    // 逐个尝试 utun0..utun7：老设备上 utun0 常被残留 daemon / 别的 VPN
    // 工具占住，connect 返回 EBUSY(os error 16)。写死 unit 0 会导致反复
    // 点连接永远失败。找一个空闲的 unit 用。
    let mut opened: Option<(std::os::fd::RawFd, String)> = None;
    let mut last_err = String::new();
    for unit in 0..8u32 {
        match utun::open_utun(unit) {
            Ok(fd) => {
                let name = utun::ifname(fd).unwrap_or_else(|_| format!("utun{unit}"));
                push_log(&format!("utun 已打开: fd={fd} ifname={name}"));
                opened = Some((fd, name));
                break;
            }
            Err(e) => {
                last_err = e.to_string();
                push_log(&format!("utun{unit} 打开失败: {e}"));
            }
        }
    }
    let (fd, real_name) = opened.ok_or_else(|| {
        anyhow::anyhow!("打开 utun 失败（0-7 全忙，需要 root）: {last_err}")
    })?;
    let mut fd_guard = UtunFd(fd); // 失败路径自动 close；成功 attach 后交出所有权
    push_log(&format!("utun fd={fd} ifname={real_name}"));

    // 与 AstralGame 一致的顺序：
    //   接口先 UP（EasyTier 要在 fd 上读写）→ 起实例 → 注 fd → 读回分配的 IP → 配地址
    //
    // DHCP 模式的地址只能从 EasyTier 读回：iOS 没有 NEIPv4Settings /
    // setTunnelNetworkSettings，协商结果没有别的途径落到内核接口上。接口没有
    // 地址时内核不会把包路由进 utun，EasyTier 也读不到东西——表现就是
    // 「进了房间但对面看不到我」。
    utun::prepare_iface(&real_name).context("激活 utun 接口失败")?;

    push_log("启动 EasyTier 实例 ...");
    let id = bridge::create_instance(&toml)?;
    push_log(&format!("instance_id = {id}"));

    bridge::attach_tun_fd(id, fd).await?;
    // fd 所有权已交给 EasyTier，禁止 guard 再 close（否则关闭它持有的 fd）
    fd_guard.0 = -1;
    push_log("tun fd 已注入：EasyTier 接管 utun");

    // 地址：DHCP 要等 EasyTier 协商完（与 AstralGame 的 get_network_status 同一来源）
    let mut final_ip = ip.clone();
    if dhcp {
        push_log("等待 EasyTier 分配虚拟 IP（DHCP）...");
        let mut got: Option<String> = None;
        for _ in 0..24 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Some(a) = bridge::my_ipv4(id).await {
                got = Some(a);
                break;
            }
        }
        match got {
            Some(a) => {
                push_log(&format!("EasyTier 已分配 {a}"));
                final_ip = a;
            }
            None => push_log(&format!(
                "12 秒内未拿到 DHCP 地址，回退到 ipv4 参数 {ip}"
            )),
        }
    }

    utun::configure_iface(&real_name, &final_ip, prefix).context("配置接口地址失败")?;
    push_log(&format!("接口已配置 {final_ip}/{prefix}"));

    let final_subnet = cli::cidr_network(&final_ip, prefix);
    // 路由是数据面的命根子：没有它，能收包但回包会走 Wi-Fi 默认网关丢掉。
    // 之前标"不致命"是错的——日志显示这台设备没有 route 命令、路由从未加上，
    // 表现正是"成员列表互通、ping 100% 丢包"。路由失败直接终止并给出原因。
    if let Err(e) = utun::add_subnet_route(&real_name, &final_subnet) {
        return Err(anyhow::anyhow!("添加路由失败（数据面无法工作）: {e:#}"));
    }
    push_log(&format!("路由已添加: {final_subnet} -> {real_name}"));
    push_log("提示：真实组网状态看上面的 tracing 日志（peer 连接 / 子网 / 分配到的 IP）");
    // 把 fd 一并交回：EasyTier 建设备时用的是 close_fd_on_drop(false)，
    // 它不会替我们关这个 fd。断开时必须自己 close，否则 utun 一直被占，
    // 下次连接就 EBUSY（0..7 逐个试也只能撑 8 次）。
    Ok((id, fd))
}

/// 断开连接。返回 0 表示已请求关闭。
#[no_mangle]
pub extern "C" fn astral_stop() -> i32 {
    // ① 立标志 + 唤醒工作线程
    begin_stop();

    // ② 有实例就异步关掉：删实例 → 关 fd → 复位状态
    let id = session_store().lock().ok().and_then(|g| g.as_ref().map(|s| s.instance_id));
    match id {
        Some(id) => {
            set_state(STATE_IDLE, "正在断开…");
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    if let Err(e) = rt.block_on(bridge::close_instance(id)) {
                        push_log(&format!("关闭实例失败: {e}"));
                    }
                }
                finish_stop();
            });
            0
        }
        None => {
            // 还在 STARTING 阶段：标志已置，bring_up 完成后会自己清理
            finish_stop();
            0
        }
    }
}

/// 当前状态（0=idle 1=starting 2=running 3=error）
#[no_mangle]
pub extern "C" fn astral_state() -> u8 {
    STATE.load(Ordering::SeqCst)
}

/// 状态文本（静态内存，调用方不要 free）
#[no_mangle]
pub extern "C" fn astral_state_text() -> *const c_char {
    state_text_store().lock().unwrap().as_ptr()
}

/// 累积日志（静态内存，调用方不要 free）
#[no_mangle]
pub extern "C" fn astral_log() -> *const c_char {
    log_c_store().lock().unwrap().as_ptr()
}

/// 清空日志
#[no_mangle]
pub extern "C" fn astral_clear_log() {
    if let Ok(mut s) = log_store().lock() {
        s.clear();
    }
    if let Ok(mut c) = log_c_store().lock() {
        *c = CString::new("").unwrap();
    }
}

/// 是否自动分配虚拟 IP（DHCP）。
///
/// 官方默认开启，与安卓/Windows 版互通时最稳（避免固定 IP 撞车）。
/// GUI 里是一个开关，调这里设置；下次 astral_start 生效。
static DHCP_ON: AtomicU8 = AtomicU8::new(0);

#[no_mangle]
pub extern "C" fn astral_set_dhcp(on: u8) {
    DHCP_ON.store(if on != 0 { 1 } else { 0 }, Ordering::SeqCst);
    push_log(&format!("自动分配虚拟IP: {}", on != 0));
}

/// 是否走 QUIC（false = TCP，官方默认）。
static QUIC_ON: AtomicU8 = AtomicU8::new(0);

#[no_mangle]
pub extern "C" fn astral_set_quic(on: u8) {
    QUIC_ON.store(if on != 0 { 1 } else { 0 }, Ordering::SeqCst);
    push_log(&format!("传输协议: {}", if on != 0 { "QUIC" } else { "TCP" }));
}
