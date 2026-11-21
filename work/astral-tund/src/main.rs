//! astral-tund —— Astral Game 的 iOS 6 命令行守护进程（无 UI）。
//!
//! 为什么存在：Astral Game 本体是 Flutter（iOS 引擎只有 arm64、最低 iOS 13），
//! 而 iOS 6 只有 armv7，且没有 NetworkExtension。所以 iOS 6 上不可能跑原 App，
//! 但**联机核心（Rust / EasyTier）可以复用**：
//!
//!   本进程（root）── ① 自己开 utun 拿 fd
//!                └─ ② 把 fd 交给 EasyTier（复用安卓 VpnService 的同一条注入路径）
//!
//! 用法（越狱 iOS 6，root）：
//!   astral-tund --name <房间名> --secret <密码> --ipv4 10.144.144.5/24 \
//!               --peer tcp://<服务器>:11010
//!   astral-tund --config /var/root/astral.toml     # 直接吃 App 导出的 TOML
//!   astral-tund --name ... --dry-run               # 只打印 TOML 与命令，不建设备

// 只保留 shim（clock_gettime 等老系统缺失符号），
// bridge/utun 现在由 lib.rs 提供，供 CLI 和原生 UI（静态库）共用。
mod shim;

use astral_tund::{bridge, utun};

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
astral-tund —— Astral Game iOS 6 命令行守护进程

必需（二选一）：
  --config <FILE>        直接使用 EasyTier TOML（App 里导出的那份）
  或
  --name <ROOM>          房间名 / network_name
  --secret <SECRET>      房间密码 / network_secret
  --ipv4 <IP/PREFIX>     本机虚拟地址，例如 10.144.144.5/24（静态，不用 DHCP）

可选：
  --peer <URI>           服务器，可重复；例如 tcp://1.2.3.4:11010
  --hostname <NAME>      节点显示名（默认 Astral）
  --unit <N>             utun 编号，默认 0（即 utun0）
  --dry-run              只打印将要生成的 TOML 与 ifconfig/route 命令
  -h, --help             显示本帮助
";

#[derive(Debug)]
struct Args {
    config: Option<PathBuf>,
    name: String,
    secret: String,
    ipv4: String,
    peers: Vec<String>,
    hostname: String,
    unit: u32,
    dry_run: bool,
    /// --daemon：以 root 常驻，监听 Unix socket 供 GUI（mobile）调用
    daemon: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        config: None,
        name: String::new(),
        secret: String::new(),
        ipv4: String::new(),
        peers: Vec::new(),
        hostname: "Astral".to_string(),
        unit: 0,
        dry_run: false,
        daemon: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let k = argv[i].as_str();
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .with_context(|| format!("{k} 缺少参数值"))
        };
        match k {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--config" => a.config = Some(PathBuf::from(next(&mut i)?)),
            "--name" => a.name = next(&mut i)?,
            "--secret" => a.secret = next(&mut i)?,
            "--ipv4" => a.ipv4 = next(&mut i)?,
            "--peer" => a.peers.push(next(&mut i)?),
            "--hostname" => a.hostname = next(&mut i)?,
            "--unit" => a.unit = next(&mut i)?.parse().context("--unit 需要整数")?,
            "--dry-run" => a.dry_run = true,
            "--daemon" => a.daemon = true,
            other => bail!("未知参数 {other}（-h 看帮助）"),
        }
        i += 1;
    }
    if a.config.is_none() && !a.daemon {
        if a.name.is_empty() || a.secret.is_empty() || a.ipv4.is_empty() {
            bail!("需要 --config，或同时给 --name/--secret/--ipv4（-h 看帮助）");
        }
    }
    Ok(a)
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 与 `lib/data/services/p2p_config_service.dart` 的 buildTomlConfig 对齐，
/// 但固定 ipv4（关掉 DHCP）——这样守护进程一启动就能确定地址、立刻配接口。
fn build_toml(a: &Args) -> String {
    let peers: String = a
        .peers
        .iter()
        .map(|p| format!("[[peer]]\nuri = \"{}\"\n", esc(p.trim())))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"instance_name = "AstralGame"
hostname = "{host}"
dhcp = false
dhcpv6 = false
ipv4 = "{ipv4}"
listeners = [
    "tcp://0.0.0.0:0",
    "udp://0.0.0.0:0",
]

[network_identity]
network_name = "{name}"
network_secret = "{secret}"

{peers}
[flags]
disable_p2p = false
data_compress_algo = 2
default_protocol = "tcp"
dev_name = "ASGame"
disable_kcp_input = true
disable_relay_kcp = true
enable_relay_foreign_network_kcp = false
disable_quic_input = true
disable_relay_quic = true
enable_relay_foreign_network_quic = false
"#,
        host = esc(&a.hostname),
        ipv4 = esc(&a.ipv4),
        name = esc(&a.name),
        secret = esc(&a.secret),
        peers = peers,
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = parse_args()?;

    if a.daemon {
        return run_daemon().await;
    }

    let toml = match &a.config {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("读取配置文件失败: {}", p.display()))?,
        None => build_toml(&a),
    };

    let ifname = format!("utun{}", a.unit);
    let (ip, prefix) = match a.ipv4.split_once('/') {
        Some((ip, p)) => (ip.to_string(), p.parse::<u8>().unwrap_or(24)),
        None => (a.ipv4.clone(), 24u8),
    };
    let subnet = cidr_network(&ip, prefix);

    // ⓪ 启动即探测：这条日志直接回答"这台设备上到底有没有 ifconfig / route"。
    //    探测只做一次，结果缓存；地址配置优先走 ioctl，不依赖这些命令。
    let t = utun::tools();

    if a.dry_run {
        println!("===== EasyTier TOML =====");
        println!("{toml}");
        println!("===== 设备配置计划 =====");
        println!("接口 {ifname}  地址 {ip}/{prefix}  路由 {subnet}");
        match &t.ifconfig {
            Some(p) => println!(
                "地址: ioctl 优先；失败则 {p} {ifname} inet {ip} {ip} netmask ... up",
                p = p.display()
            ),
            None => println!("地址: 纯 ioctl（未找到 ifconfig，也不需要）"),
        }
        match &t.route {
            Some(p) => println!(
                "路由: {p} -n add -net {subnet} -interface {ifname}",
                p = p.display()
            ),
            None => println!(
                "路由: 未找到 route 命令（不影响：已改用 PF_ROUTE 内核接口加路由）。\
                 想手动诊断可 Cydia 装 network-cmds（含 ping/netstat/ifconfig）"
            ),
        }
        return Ok(());
    }

    // ① 开 utun（需要 root）
    let fd = utun::open_utun(a.unit).context("打开 utun 失败（是否 root？）")?;
    let real_name = utun::ifname(fd).unwrap_or_else(|_| ifname.clone());
    println!("[main] utun fd={fd} ifname={real_name}");

    // ② 先配地址（后续 EasyTier 直接用这个地址收发）
    utun::configure_iface(&real_name, &ip, prefix).context("配置接口地址失败")?;
    if let Err(e) = utun::add_subnet_route(&real_name, &subnet) {
        println!("[main] 加路由失败（不致命，可按需手动加）: {e}");
    }

    // ③ 启动 EasyTier 实例（直接调 EasyTier，不经过 flutter_rust_bridge）
    println!("[main] starting EasyTier instance ...");
    let instance_id = bridge::create_instance(&toml)?;
    println!("[main] instance_id = {instance_id}");

    // ④ 把 utun fd 注入（与安卓 VpnService 完全同一条路径）
    bridge::attach_tun_fd(instance_id, fd).await?;
    println!("[main] tun fd attached -> 组网已启动（Ctrl-C 退出）");

    // ⑤ 常驻；收到 Ctrl-C 时优雅关闭
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("[main] 退出中 ...");
            if let Err(e) = bridge::close_instance(instance_id).await {
                println!("[main] 关闭实例失败: {e}");
            }
        }
        _ = tokio::time::sleep(Duration::from_secs(u64::MAX / 2)) => {}
    }
    Ok(())
}

/// 由 "ip/prefix" 算出网络号，用于 route（例：10.144.144.5/24 -> 10.144.144.0/24）。
fn cidr_network(ip: &str, prefix: u8) -> String {
    let octets: Vec<u32> = ip.split('.').filter_map(|s| s.parse().ok()).collect();
    if octets.len() != 4 {
        return format!("{ip}/{prefix}");
    }
    let v = (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3];
    let bits: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    let n = v & bits;
    format!(
        "{}.{}.{}.{}/{}",
        (n >> 24) & 0xff,
        (n >> 16) & 0xff,
        (n >> 8) & 0xff,
        n & 0xff,
        prefix
    )
}

// ============================================================ daemon 模式
//
// 为什么要 daemon：
//   iOS 上 GUI App 由 SpringBoard 以 mobile 用户启动，**无法创建 utun**
//   （需要 root），而 root 身份的 GUI 又连不上 WindowServer。
//   所以拆两段：本进程以 root 常驻（LaunchDaemon），App 通过 Unix socket
//   发命令、拉状态。deb 的 postinst 会装好 plist 并 load。
//
// 协议（单行命令 → 单段文本回复）：
//   START <room> <secret> <ipv4> <peer>   → rc=<int>
//   STOP                                   → rc=<int>
//   STATUS                                 → state=<0..3>\ntext=<...>
//   LOG                                    → 累积日志原文
//   CLEAR                                  → rc=0

const SOCK_PATH: &str = "/var/run/astral.sock";

async fn run_daemon() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    // 尽早装 subscriber：daemon 一启动就能看到 EasyTier 的协议层日志
    astral_tund::init_tracing();
    println!("[daemon] 启动（PID {}），日志 -> /var/log/astral-tund.log", std::process::id());

    let _ = std::fs::remove_file(SOCK_PATH);
    let listener = UnixListener::bind(SOCK_PATH)
        .with_context(|| format!("bind {SOCK_PATH} 失败（需要 root）"))?;
    // App 以 mobile 运行，必须放宽权限才能连
    std::fs::set_permissions(SOCK_PATH, std::fs::Permissions::from_mode(0o666)).ok();
    println!("[daemon] listening on {SOCK_PATH}");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[daemon] accept: {e}");
                continue;
            }
        };
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            let mut reader = BufReader::new(rd);
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let resp = handle_daemon_cmd(line.trim_end_matches(['\r', '\n'])).await;
            let _ = wr.write_all(resp.as_bytes()).await;
            let _ = wr.flush().await;
        });
    }
}

fn cstring(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_else(|_| std::ffi::CString::new("").unwrap())
}

async fn handle_daemon_cmd(line: &str) -> String {
    // 只按空格切 5 段；**不能先 trim 整行**——peer 为空时命令形如
    // "START room secret 10.144.144.5/24 " （末尾一个空格），trim 掉就只剩
    // 4 段，START 会掉进 default 分支返回 "ERR unknown command: START"，
    // 表现就是 UI 一直在"连接中"而实例从未启动。
    let parts: Vec<&str> = line.splitn(5, ' ').collect();
    match *parts.first().unwrap_or(&"") {
        // peer 允许为空：只给 4 段（START room secret ipv4）也算合法
        "START" if parts.len() >= 4 => {
            let room = cstring(parts[1].trim());
            let secret = cstring(parts[2].trim());
            let ipv4 = cstring(parts[3].trim());
            let peer = cstring(parts.get(4).map(|s| s.trim()).unwrap_or(""));
            // 注意 secret 不打印明文
            println!(
                "[daemon] START room={:?} ipv4={:?} peer={:?}",
                parts[1], parts[3], peer.to_string_lossy()
            );
            let rc = astral_tund::astral_start(
                room.as_ptr(),
                secret.as_ptr(),
                ipv4.as_ptr(),
                peer.as_ptr(),
            );
            format!("rc={rc}\n")
        }
        "HOST" if parts.len() >= 2 => {
            // HOST <昵称>
            let name = cstring(parts[1]);
            astral_tund::astral_set_hostname(name.as_ptr());
            "rc=0\n".to_string()
        }
        "DHCP" if parts.len() >= 2 => {
            // DHCP 1 / DHCP 0 —— 是否自动分配虚拟 IP
            let on: u8 = if parts[1].trim() == "1" { 1 } else { 0 };
            astral_tund::astral_set_dhcp(on);
            "rc=0\n".to_string()
        }
        "QUIC" if parts.len() >= 2 => {
            // QUIC 1 / QUIC 0 —— 传输协议（1=QUIC, 0=TCP）
            let on: u8 = if parts[1].trim() == "1" { 1 } else { 0 };
            astral_tund::astral_set_quic(on);
            "rc=0\n".to_string()
        }
        // STOP —— **同步**完整清理：删实例 → 关 fd → 复位状态，全部完成后
        // 才回 rc=0。之前走 astral_stop 的异步路径，UI 断开立刻重连时会跟
        // 新实例竞态：旧实例没删完、新实例已入网（房间两个节点），
        // 甚至误关新会话的 fd。
        "STOP" => {
            astral_tund::begin_stop();
            if let Some(id) = astral_tund::current_instance_id() {
                if let Err(e) = astral_tund::bridge::close_instance(id).await {
                    println!("[daemon] 关闭实例失败: {e}");
                }
            }
            if let Some(fd) = astral_tund::take_session_fd() {
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                    println!("[daemon] 已关闭 utun fd={fd}");
                }
            }
            astral_tund::finish_stop();
            "rc=0\n".to_string()
        }
        "STATUS" => {
            let st = astral_tund::astral_state();
            let txt = unsafe {
                std::ffi::CStr::from_ptr(astral_tund::astral_state_text())
            }
            .to_string_lossy()
            .into_owned();
            format!("state={st}\ntext={txt}\n")
        }
        // PEERS —— 房间成员列表（谁在房间里、各自的虚拟 IP）。
        // 数据与 AstralGame 的 get_network_status 同源：network_info()
        //   → 本机 my_node_info + 其它节点 routes。
        "PEERS" => match astral_tund::current_instance_id() {
            None => "ERR not running\n".to_string(),
            Some(id) => match astral_tund::bridge::members(id).await {
                None => "ERR instance not ready\n".to_string(),
                Some(list) => {
                    let mut s = format!("count={}\n", list.len());
                    for m in &list {
                        s.push_str(&m.to_line());
                        s.push('\n');
                    }
                    s
                }
            },
        },
        "LOG" => unsafe {
            std::ffi::CStr::from_ptr(astral_tund::astral_log())
                .to_string_lossy()
                .into_owned()
        },
        "CLEAR" => {
            astral_tund::astral_clear_log();
            "rc=0\n".to_string()
        }
        other => format!("ERR unknown command: {other}\n"),
    }
}
