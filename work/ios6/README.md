# iOS 6 构建说明（GitHub Actions / 本地 Mac）

## 一句话

**云编译能提供 Mac，但现成 runner 出不了 iOS 6 产物** —— 必须自带两样东西。

## 为什么现成镜像不行（两条硬原因，已核实）

| # | 事实 | 来源 |
|---|---|---|
| 1 | runner 的 Xcode 只有 15.0–16.2，安装的 iOS SDK 只有 **17.0–18.2**，全是 arm64-only。没有 armv7 的 libSystem 桩 → 链接必失败 | `actions/runner-images` 的 macos-14 Readme |
| 2 | 当前 Rust 的平台上支持列表里**已经没有 `armv7-apple-ios`**，只剩 `armv7s-apple-ios`（Tier 3，A6 的 Swift 扩展） | `rust-lang/rust` 的 platform-support.md |

## 需要自备的两样

### ① iPhoneOS6.1.sdk

只在老 Xcode 里有。从你自己的 Mac 上取出（Xcode 4.6）：

```bash
SDK=/Applications/Xcode.app/Contents/Developer/Platforms/iPhoneOS.platform/Developer/SDKs/iPhoneOS6.1.sdk
ls "$SDK"                       # 应能看到 SDKSettings.plist / usr/lib/libSystem.B.dylib
tar -czf iPhoneOS6.1.sdk.tar.gz -C "$(dirname $SDK)" iPhoneOS6.1.sdk
```

然后二选一：

- **放仓库**：`sdk/iPhoneOS6.1.sdk/`（大于 100MB 时用 Git LFS 或 Release 附件）
- **用 secret**：把 tar.gz 传到任意可直链的位置，配 `IOS6_SDK_URL`

> 注意：6.1 SDK 里的 libSystem 是二进制 `.dylib` 桩（那时还没有 `.tbd` 文本格式），
> `ld` 能直接吃，配合 `-isysroot` 即可。

### ② nightly + build-std

Tier 3 目标没有预编译 std，必须 `-Z build-std`。workflow 里已经装了 nightly 和 `rust-src`。

## 关键技巧：从 armv7s 派生纯 armv7

`armv7s` 是 ARMv7 + A6 的 Swift 扩展，**A5 及更早的设备（4S / iPad 2/3 / touch 5）不认**。
如果直接编 `armv7s`，产物只能在 iPhone 5 / iPad 4 上跑。

所以 `build-ci.sh` 的做法是：

```bash
rustc -Z unstable-options --print target-spec-json --target armv7s-apple-ios > armv7s.json
# 把 cpu: swift -> generic，features 改成标准 v7+thumb2+vfp3+neon
```

然后用这份自定义 JSON 当 `--target`。这样得到的是**标准 armv7**，
覆盖**全部 iOS 6 设备**（3GS 到 iPhone 5）。

## 用法

### 云端

1. 把 `workflow-build-ios6.yml` 复制到仓库的 `.github/workflows/build-ios6.yml`
2. 准备好 ①（放 `sdk/` 或配 `IOS6_SDK_URL`）
3. Actions 页面点 `Run workflow`

产物：`astral-tund-ios6-armv7`（已 `ldid -S` 签名）。

### 本地 Mac（更快，推荐先用它调通）

```bash
# 让 SDK 就位
mkdir -p work/ios6/sdk && cp -R /path/to/iPhoneOS6.1.sdk work/ios6/sdk/
# 构建
chmod +x work/ios6/build-ci.sh
REPO_ROOT=$PWD ./work/ios6/build-ci.sh
```

## 已知的坑

| 现象 | 原因 / 处理 |
|---|---|
| `ld: building for iOS, but linking in object file built for...` | `SDKROOT` 指错了，或用了 runner 自带的 SDK |
| `error: could not find native static library 'System'` | SDK 里没有 armv7 桩 —— 说明不是 6.1 SDK |
| `armv7s` 编出来但 4S 上跑不了 | 没走自定义 JSON，直接用了 `armv7s-apple-ios` |
| `-Z build-std` 报 nightly 特性 | 忘了 `rustup default nightly`，或需要 `RUSTC_BOOTSTRAP=1` |
| 链接时 `undefined symbol: ___sincos_stret` 之类 | 那是**旧 iOS 缺新 libc 函数**的通用问题，等真机跑起来再按调用点处理（参考迷你世界项目的做法） |
| `ldid -S` 后仍装不上 | 越狱环境需配合 AppSync / 自签策略；本程序是 CLI，直接 scp 到 `/usr/bin` 即可 |

## 现状

- 脚本与 workflow 已就绪，**但尚未在真实 runner 上跑过**（我这边是 Windows，无法执行 macOS 构建）。
- 第一次跑建议先看 `Show runner toolchain` 和 `Provide iOS 6.1 SDK` 两步的输出。
- 若 `build-std` 阶段失败，把完整日志发出来，通常是自定义 target JSON 少了某个字段
  （可直接拿 `armv7s-apple-ios` 的 spec 对照补齐）。
