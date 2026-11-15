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
/// `dhcp = true` 时由 EasyTier 自动分配虚拟 IP（官方默认行为，
/// 也最容易和安卓/Windows 版互通）；false 时写死 `ipv4`。
pub fn build_toml(
    hostname: &str,
    room: &str,
    secret: &str,
    ipv4: &str,
    peers: &[String],
    dhcp: bool,
) -> String {
    let peer_block: String = peers
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("[[peer]]\nuri = \"{}\"\n", esc(p.trim())))
        .collect::<Vec<_>>()
        .join("\n");

    // DHCP 模式不写 ipv4（写了会被忽略/冲突），由 EasyTier 分配
    let addr_block = if dhcp {
        "dhcp = true\ndhcpv6 = false".to_string()
    } else {
        format!("dhcp = false\ndhcpv6 = false\nipv4 = \"{}\"", esc(ipv4))
    };

    format!(
        r#"instance_name = "AstralGame"
hostname = "{host}"
{addr_block}
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
        addr_block = addr_block,
        name = esc(room),
        secret = esc(secret),
        peers = peer_block,
    )
}

/// UI 用的简化版：单个 peer
#[allow(dead_code)]
pub fn build_toml_simple(room: &str, secret: &str, ipv4: &str, peer: &str) -> String {
    build_toml(
        "AstralGame",
        room,
        secret,
        ipv4,
        &[peer.to_string()],
        false,
    )
}