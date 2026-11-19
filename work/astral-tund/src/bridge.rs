//! 直接对接 EasyTier 的最小封装。
//!
//! 为什么不用 AstralGame 的 `rust_lib_astral_game::api::p2p`：
//! 那个 crate 依赖 `flutter_rust_bridge`，而 FRB 2.12 在 Apple 上依赖 `oslog`
//! —— `oslog` 需要 `os/log.h`（iOS 10+），iOS 6 根本没有这个头文件，编译必失败。
//! CLI 不需要 Flutter 桥，所以这里把需要的三件事直接实现：
//! `create_instance` / `attach_tun_fd` / `close_instance`。
//!
//! 逻辑与 `rust_lib_astral_game/src/api/p2p.rs` 保持一致
//! （含 secure_mode 的密钥归一化，否则连公共节点会报 local private key is not set）。

use anyhow::{Context, Result};
use easytier::common::config::{
    normalize_secure_mode_config, ConfigFileControl, ConfigLoader, TomlConfigLoader,
};
use easytier::instance::factory::{native_cli_instance_manager, NativeInstanceManager};
use lazy_static::lazy_static;
use uuid::Uuid;

lazy_static! {
    static ref MANAGER: NativeInstanceManager =
        native_cli_instance_manager(false, 0).expect("创建 EasyTier 实例管理器失败");
}

/// 解析 TOML、归一化 secure_mode、启动实例，返回 instance id。
pub fn create_instance(toml: &str) -> Result<Uuid> {
    let cfg = TomlConfigLoader::new_from_str(toml)
        .map_err(|e| anyhow::anyhow!("TOML 解析失败: {e}"))?;
    let id = cfg.get_id();

    if let Some(sm) = cfg.get_secure_mode() {
        if sm.enabled {
            let had_priv = sm.local_private_key.is_some();
            let processed = normalize_secure_mode_config(sm)
                .map_err(|e| anyhow::anyhow!("secure_mode 处理失败: {e}"))?;
            cfg.set_secure_mode(Some(processed));
            if !had_priv {
                println!("[bridge] secure_mode: 已生成 local keypair（TOML 里没写）");
            }
        }
    }

    let net = cfg.get_network_identity();
    let peers = cfg
        .get_peers()
        .iter()
        .map(|p| p.uri.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "[bridge] network={} hostname={:?} keep_ipv4={:?} peers=[{}]",
        net.network_name,
        // 玩家名排查：config 层的 hostname 实际值。若这里有值而成员列表仍
        // 显示空名，问题就在 EasyTier global_ctx 的 runtime 覆盖链
        // （global_ctx.rs:177：runtime.node.hostname 会覆盖 TOML）。
        cfg.get_hostname(),
        cfg.get_ipv4().map(|i| i.to_string()),
        peers
    );

    MANAGER
        .run_network_instance(cfg, ConfigFileControl::STATIC_CONFIG)
        .map_err(|e| anyhow::anyhow!("启动实例失败: {e}"))?;

    Ok(id)
}

/// 把宿主的 tun fd 交给 EasyTier（与安卓 VpnService 同一条路径）。
pub fn attach_tun_fd(id: Uuid, fd: i32) -> Result<()> {
    MANAGER
        .attach_tun_fd(id, fd)
        .map_err(|e| anyhow::anyhow!("attach_tun_fd 失败: {e}"))
        .context("EasyTier 未能接管 tun fd")
}

/// 读回本机被 EasyTier 分配的虚拟 IPv4（DHCP 协商结果），去掉 `/prefix`。
///
/// 数据来源与 AstralGame 的 `get_network_status` 完全一致：
///   `MANAGER.network_info(id)` → `my_node_info.virtual_ipv4`
/// （安卓端先由 VpnService.Builder 把地址配进 tun；iOS 没有 NEIPv4Settings，
///   只能自己读回来再 ioctl，这是唯一等价做法。）
///
/// 尚未协商出地址时返回 `None`，调用方应轮询等待。
pub async fn my_ipv4(id: Uuid) -> Option<String> {
    let info = MANAGER.network_info(id).await?;
    let addr = info
        .my_node_info
        .as_ref()?
        .virtual_ipv4
        .as_ref()?
        .address
        .clone()?;
    // proto 的 Ipv4Addr.address 不是 String，必须先 to_string()
    // （AstralGame 里就是 `.map(|ip| ip.to_string())`，这里同样处理）
    let raw = addr.to_string();
    let ip = raw.split('/').next().unwrap_or(&raw).trim().to_string();
    if ip.is_empty() || ip == "0.0.0.0" {
        None
    } else {
        Some(ip)
    }
}

/// 房间成员快照（用于 UI 显示「谁在房间里、各自虚拟 IP」）。
pub struct Member {
    /// 是否本机（route.inst_id == "local" 或 peer_id == 0）
    pub local: bool,
    pub hostname: String,
    pub ipv4: String,
    pub peer_id: u32,
    /// 到该节点的跳数（0/1 = 直连）。与 KVNodeInfo.cost 同类型（i32）。
    pub cost: i32,
    /// EasyTier 版本（对端上报）
    pub version: String,
    /// UDP NAT 类型（对端探测结果，如 Unknown / OpenCone）
    pub nat: String,
    /// 当前连接协议（udp / tcp / quic / wg…），取首选
    pub conn_type: String,
}

impl Member {
    /// 两行可读文本（第二行 = 设备信息行，对齐安卓成员卡片的副标题）：
    ///   [我] Astral  100.100.100.114  (peer=1234, cost=0)
    ///        2.7.0-xxx · NAT Unknown · udp
    /// 注意：安卓那行里的「地区运营商 / 操作系统 / 防火墙」来自 peer-RPC
    /// （user.getInfo 主动上报），这里暂时显示路由表里有的字段，
    /// RPC 管道移植后补齐。
    pub fn to_line(&self) -> String {
        let me = if self.local { "[我] " } else { "" };
        let name = if self.hostname.is_empty() {
            format!("peer{}", self.peer_id)
        } else {
            self.hostname.clone()
        };
        let ip = if self.ipv4.is_empty() {
            "-".to_string()
        } else {
            self.ipv4.clone()
        };
        let ver = if self.version.is_empty() { "-" } else { &self.version };
        let nat = if self.nat.is_empty() { "-" } else { &self.nat };
        let conn = if self.conn_type.is_empty() { "-" } else { &self.conn_type };
        format!(
            "{me}{name}  {ip}  (peer={}, cost={})\n    {ver} · NAT {nat} · {conn}",
            self.peer_id, self.cost
        )
    }
}

/// 读回房间成员列表。
///
/// 逐条照 AstralGame 的 `peer_route_pairs_from_info` 来做，不是自己拼：
///   1. 优先用 `info.peer_route_pairs`
///   2. 为空时用 `list_peer_route_pair(peers, routes)` 把两边配对
///      （只遍历 routes 会丢掉 peer 侧的连接信息）
///   3. 本机那一行的判据是 `route.inst_id == "local"` 或 `peer_id == 0`
///      （LOCAL_SYNTHETIC_PEER_ID）
pub async fn members(id: Uuid) -> Option<Vec<Member>> {
    let info = MANAGER.network_info(id).await?;
    Some(members_from_info(info))
}

fn members_from_info(info: easytier::proto::api::manage::NetworkInstanceRunningInfo) -> Vec<Member> {
    use easytier::proto::api::instance::list_peer_route_pair;

    let pairs = if info.peer_route_pairs.is_empty() {
        list_peer_route_pair(info.peers.clone(), info.routes.clone())
    } else {
        info.peer_route_pairs.clone()
    };

    let mut out: Vec<Member> = Vec::new();
    for p in &pairs {
        let Some(r) = p.route.as_ref() else { continue };
        // 设备信息行字段：nat / conn 与 AstralGame 的 KVNodeInfo 同源
        // （p.get_udp_nat_type / p.get_conn_protos，见 p2p.rs:815-830 的用法）
        let nat = p.get_udp_nat_type();
        let conn = p
            .get_conn_protos()
            .and_then(|v| v.first().cloned())
            .unwrap_or_default();
        let ver = r.version.clone();
        out.push(Member {
            local: r.inst_id == "local" || r.peer_id == 0,
            hostname: r.hostname.clone(),
            ipv4: r
                .ipv4_addr
                .as_ref()
                .and_then(|a| a.address.clone())
                .map(|ip| ip.to_string())
                .unwrap_or_default(),
            peer_id: r.peer_id,
            cost: r.cost,
            version: ver,
            nat,
            conn_type: conn,
        });
    }

    // 兜底：配对结果里没有本机时，用 my_node_info 补一条，
    // 保证「[我] → 虚拟 IP」这行一定出现在 UI 上。
    if !out.iter().any(|m| m.local) {
        if let Some(me) = info.my_node_info.as_ref() {
            out.insert(
                0,
                Member {
                    local: true,
                    hostname: me.hostname.clone(),
                    ipv4: me
                        .virtual_ipv4
                        .as_ref()
                        .and_then(|a| a.address.clone())
                        .map(|ip| ip.to_string())
                        .unwrap_or_default(),
                    peer_id: me.peer_id,
                    cost: 0,
                    version: me.version.clone(),
                    nat: "-".to_string(),
                    conn_type: "local".to_string(),
                },
            );
        }
    }
    out
}

/// 关闭实例。注意 `delete_network_instances` 返回被删除的 id 列表。
pub async fn close_instance(id: Uuid) -> Result<()> {
    MANAGER
        .delete_network_instances(vec![id])
        .await
        .map(|_ids| ())
        .map_err(|e| anyhow::anyhow!("关闭实例失败: {e}"))
}
