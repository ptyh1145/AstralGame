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
        "[bridge] network={} keep_ipv4={:?} peers=[{}]",
        net.network_name,
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

/// 关闭实例。注意 `delete_network_instances` 返回被删除的 id 列表。
pub async fn close_instance(id: Uuid) -> Result<()> {
    MANAGER
        .delete_network_instances(vec![id])
        .await
        .map(|_ids| ())
        .map_err(|e| anyhow::anyhow!("关闭实例失败: {e}"))
}
