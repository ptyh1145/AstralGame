# Astral Game → iOS 6：命令行守护进程（第一阶段）

用**无 UI 的守护进程**验证"iOS 6 能不能真的联上 Astral 的房间"。
界面（房间/好友/游戏库）属于后续阶段，不在本目录范围。

```
astral-tund (root)
 ├─ 打开 utun（PF_SYSTEM / com.apple.net.utun_control）
 ├─ ifconfig/route 配置虚拟地址与网段
 ├─ 启动 EasyTier 实例（复用 AstralGame 的 Rust 层）
 └─ set_tun_fd(fd)   ← 与安卓 VpnService 同一条注入路径
```

## 为什么可行（关键事实）

| 事实 | 出处 |
|---|---|
| Flutter iOS 引擎只有 arm64，本项目 `MinimumOSVersion = 13.0` | `ios/Flutter/AppFrameworkInfo.plist` |
| iOS 6 只有 armv7，且**没有 NetworkExtension**（iOS 8+） | 系统能力 |
| EasyTier 已内置 iOS 支持 | `easytier/src/instance/virtual_nic.rs` 的 `target_os = "ios"` 分支 |
| iOS 分支手工补 4 字节 AF 头（`packet_information(false)`），**正是 utun 的报文约定** | 同上，第 613–626 行 |
| AstralGame 已暴露 `set_tun_fd(instance_id, fd)` | `rust/src/api/p2p.rs:476` |
| Rust 配置是**现成 TOML**，room→配置的推导在 Dart 层 | `create_instance(config_toml)` |

结论：**核心不用改**，只要把"谁来提供 tun fd"从 NetworkExtension 换成自建 utun。

## 构建（Mac）

前置：Xcode + iOS 6.1 SDK、Rust（见 `../build_ios6.sh` 里的版本说明）、
iOS 6.1 armv7 的 `libSystem.tbd` 等。

```bash
cd work/astral-tund
cargo build --release --target armv7-apple-ios     # 目标可用性见脚本里的检查
```

> ⚠️ `armv7-apple-ios` 在新版 Rust 里已不是一级目标，很可能需要老工具链
> 或 nightly + `-Z build-std`。脚本会把三种情况都探测一遍并给出提示。

## 运行（越狱 iOS 6，root）

```bash
# 直接吃 App 导出的 TOML（最稳，字段与 App 完全一致）
/var/root/astral-tund --config /var/root/astral.toml

# 或者命令行拼配置（静态 IP，不用 DHCP）
/var/root/astral-tund \
  --name MyRoom --secret s3cr3t \
  --ipv4 10.144.144.5/24 \
  --peer tcp://<服务器地址>:11010

# 只看会生成什么，不建设备
/var/root/astral-tund --name MyRoom --secret s3cr3t --ipv4 10.144.144.5/24 --dry-run
```

验证连通性：

```bash
ifconfig utun0                     # 看到 10.144.144.5
ping 10.144.144.1                  # 房间里另一端（EasyTier 虚拟网段）
```

## 已知待办（按优先级）

1. **`armv7-apple-ios` 目标可用性** —— 第一个要实测的点，决定要不要退老工具链。
2. **utun 编号冲突** —— 若 utun0 被占用，用 `--unit 3` 之类换一个。
3. **路由范围** —— 目前只加虚拟网段；要转发游戏局域网广播还需要 `lan_nat` 等价逻辑。
4. **DHCP 模式** —— 本骨架固定静态 IP；若要 DHCP，需要启动后读 `get_network_status`
   拿到分配到的地址再 ifconfig。
5. **开机自启** —— 越狱环境用 launchd plist（`/Library/LaunchDaemons/`）。

## 与后续阶段的关系

- 本阶段通 = "iOS 6 设备能加入 Astral 房间并被同网段 ping 通"，核心风险清零。
- 之后再决定要不要做 UIKit 界面（那才是真正的大工程）。
