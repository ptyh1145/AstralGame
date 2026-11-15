//! CLI 与 UI 共用的配置构造逻辑。

/// 转义放进 TOML 双引号字符串的内容
pub fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 由 "ip/prefix" 算出网络号（例：10.144.144.5/24 -> 10.144.144.0/24）
pub fn cidr_network(ip: &str, prefix: u8) -> String {
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

/// 生成 EasyTier 的 TOML 配置（与 Astral Game 里 buildTomlConfig 对齐）。
///
/// 与 CLI 版一致：**固定 ipv4、关掉 DHCP**，这样启动后能立刻配接口。
pub fn build_toml(
    hostname: &str,
    room: &str,
    secret: &str,
    ipv4: &str,
    peers: &[String],
) -> String {
    let peer_block: String = peers
        .iter()
        .filter(|p| !p.trim().is_empty())
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
        host = esc(hostname),
        ipv4 = esc(ipv4),
        name = esc(room),
        secret = esc(secret),
        peers = peer_block,
    )
}

/// UI 用的简化版：单个 peer
pub fn build_toml_simple(room: &str, secret: &str, ipv4: &str, peer: &str) -> String {
    build_toml(
        "AstralGame",
        room,
        secret,
        ipv4,
        &[peer.to_string()],
    )
}