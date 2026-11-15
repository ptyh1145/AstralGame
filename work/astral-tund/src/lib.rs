//! 把联机核心暴露成 C ABI，给原生 UI（Objective-C）调用。
//!
//! 设计要点：
//!   * `astral_start` **立即返回**，真正的连接过程在后台 tokio runtime 线程里跑；
//!     UI 通过轮询 `astral_state` / `astral_log` 显示进度。
//!   * 状态和日志都放在全局静态里，返回 `*const c_char`（内存常驻，UI 不用释放）。
//!   * 只暴露 4 个函数，避免把 Rust 类型泄漏给 ObjC。

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

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
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

fn session_store() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
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

    if STATE.load(Ordering::SeqCst) == STATE_RUNNING || STATE.load(Ordering::SeqCst) == STATE_STARTING {
        push_log("已经在运行/启动中，忽略重复请求");
        return -2;
    }

    set_state(STATE_STARTING, "启动中");

    // 后台线程跑 tokio runtime，UI 线程不阻塞
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                set_state(STATE_ERROR, "创建 runtime 失败");
                push_log(&format!("创建 tokio runtime 失败: {e}"));
                return;
            }
        };

        let result: Result<uuid::Uuid> = rt.block_on(async { bring_up(&room, &secret, &ipv4, &peer).await });

        match result {
            Ok(id) => {
                push_log(&format!("实例已启动: {id}"));
                // 让 runtime 常驻，UI 侧靠 astral_stop 结束
                *session_store().lock().unwrap() = Some(Session {
                    instance_id: id,
                    runtime: None, // runtime 由本线程持有，见下面 park
                });
                set_state(STATE_RUNNING, "已连接");
                // 保持线程与 runtime 存活
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                set_state(STATE_ERROR, "连接失败");
                push_log(&format!("启动失败: {e:#}"));
            }
        }
        drop(rt);
    });

    0
}

/// 真正建立连接：开 utun → 配地址 → 建 EasyTier 实例 → 注入 fd
async fn bring_up(room: &str, secret: &str, ipv4: &str, peer: &str) -> Result<uuid::Uuid> {
    let host = hostname_store().lock().unwrap().clone();
    let toml = cli::build_toml(&host, room, secret, ipv4, &[peer.to_string()]);

    let (ip, prefix) = match ipv4.split_once('/') {
        Some((ip, p)) => (ip.to_string(), p.parse::<u8>().unwrap_or(24)),
        None => (ipv4.to_string(), 24u8),
    };
    let subnet = cli::cidr_network(&ip, prefix);

    push_log("打开 utun ...");
    let fd = utun::open_utun(0).context("打开 utun 失败（需要 root）")?;
    let real_name = utun::ifname(fd).unwrap_or_else(|_| "utun0".to_string());
    push_log(&format!("utun fd={fd} ifname={real_name}"));

    utun::configure_iface(&real_name, &ip, prefix).context("配置接口地址失败")?;
    push_log(&format!("接口已配置 {ip}/{prefix}"));
    if let Err(e) = utun::add_subnet_route(&real_name, &subnet) {
        push_log(&format!("加路由失败（不致命）: {e}"));
    }

    push_log("启动 EasyTier 实例 ...");
    let id = bridge::create_instance(&toml)?;
    push_log(&format!("instance_id = {id}"));

    bridge::attach_tun_fd(id, fd)?;
    push_log("tun fd 已注入，组网已启动");
    Ok(id)
}

/// 断开连接。返回 0 表示已请求关闭。
#[no_mangle]
pub extern "C" fn astral_stop() -> i32 {
    if STATE.load(Ordering::SeqCst) != STATE_RUNNING {
        set_state(STATE_IDLE, "已停止");
        return 0;
    }
    let id = session_store().lock().ok().and_then(|g| g.as_ref().map(|s| s.instance_id));
    match id {
        Some(id) => {
            // 用一个临时 runtime 执行异步关闭
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    if let Err(e) = rt.block_on(bridge::close_instance(id)) {
                        push_log(&format!("关闭实例失败: {e}"));
                    }
                }
                *session_store().lock().unwrap() = None;
                set_state(STATE_IDLE, "已停止");
            });
            0
        }
        None => {
            set_state(STATE_IDLE, "已停止");
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
