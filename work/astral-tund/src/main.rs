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
            other => bail!("未知参数 {other}（-h 看帮助）"),
        }
        i += 1;
    }
    if a.config.is_none() {
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

    if a.dry_run {
        println!("===== EasyTier TOML =====");
        println!("{toml}");
        println!("===== 将要执行的设备配置 =====");
        println!("/sbin/ifconfig {ifname} inet {ip} {ip} netmask <prefix {prefix}> up");
        println!("/sbin/route -n add -net {subnet} -interface {ifname}");
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
    bridge::attach_tun_fd(instance_id, fd)?;
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
