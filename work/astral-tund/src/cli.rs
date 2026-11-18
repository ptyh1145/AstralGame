//! CLI 与 UI 共用的配置构造逻辑。

/// 房间虚拟网段（DHCP 初始网段），与 AstralGame 的
/// `kRoomDhcpIpv4Cidr` / `kRoomDhcpIpv6Cidr` 完全一致。
///
/// EasyTier 的 `dhcp` / `dhcpv6` 字段接受 bool 或 CIDR 字符串（untagged
/// DhcpConfig）。写裸 `true` 会落到默认网段，和安卓端不在同一个虚拟网络里，
/// 表现就是「双方都进了房间、却谁也看不见谁」。
pub const DHCP_V4_CIDR: &str = "100.100.100.0/24";
pub const DHCP_V6_CIDR: &str = "fdae::/120";

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

/// 生成 EasyTier 的 TOML 配置（与 AstralGame 的 `buildTomlConfig` 对齐）。
///
/// `dhcp = true` 时写 `dhcp = "100.100.100.0/24"` + `dhcpv6 = "fdae::/120"`，
/// 由 EasyTier 在**同一虚拟网段**内协商地址（与安卓端完全一致）；
/// `false` 时写死 `ipv4`。
pub fn build_toml(
    hostname: &str,
    room: &str,
    secret: &str,
    ipv4: &str,
    peers: &[String],
    dhcp: bool,
    quic: bool,
) -> String {
    let peer_block: String = peers
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("[[peer]]\nuri = \"{}\"\n", esc(p.trim())))
        .collect::<Vec<_>>()
        .join("\n");

    // 与 AstralGame 的 p2p_config_service.dart 逐字对齐：
    //   dhcp    = "100.100.100.0/24"   ← 带网段，不是裸 true
    //   dhcpv6  = "fdae::/120"
    // 这样 iOS 与安卓落在同一虚拟网段，EasyTier 的 DHCP 才会分配可互通的地址。
    let addr_block = if dhcp {
        format!("dhcp = \"{DHCP_V4_CIDR}\"\ndhcpv6 = \"{DHCP_V6_CIDR}\"")
    } else {
        format!("dhcp = false\ndhcpv6 = false\nipv4 = \"{}\"", esc(ipv4))
    };

    // 传输协议：与官方 _flagsBlock 的 ternary 完全一致
    //   TCP  → 允许 QUIC 代理（其他节点可用 QUIC 连过来）
    //   其他 → 关闭 QUIC 输入/中继
    let (proto_name, quic_block) = if quic {
        (
            "quic",
            "disable_quic_input = true\ndisable_relay_quic = true\n\
             enable_relay_foreign_network_quic = false"
                .to_string(),
        )
    } else {
        (
            "tcp",
            "disable_quic_input = false\ndisable_relay_quic = false\n\
             enable_quic_proxy = true\nenable_relay_foreign_network_quic = true"
                .to_string(),
        )
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
default_protocol = "{proto_name}"
dev_name = "ASGame"
disable_kcp_input = true
disable_relay_kcp = true
enable_relay_foreign_network_kcp = false
{quic_block}
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
        false,
    )
}