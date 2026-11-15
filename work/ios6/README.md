# astral-tund：iOS 构建说明（GitHub Actions / 本地 Mac）

> 这份文档记录的是**实测过的真实约束**，不是设想。每一条"坑"后面都有
> 对应的真实报错，改动脚本前请先读一遍。

## 当前目标

**iOS 8.0 / armv7s / thin Mach-O**，先跑通"无 UI 守护进程 + EasyTier 联机"。

iOS 6 不是不能做，但成本高得多（原因见最后一节「能否压到 iOS 6」），
所以先交付 8.0 版本验证功能，再评估是否降级。

## 三条硬约束（决定了整个方案）

### 1. rustc 把 armv7s 的最低部署目标硬编码成 10.0

`rustc 1.102` 对 `armv7s-apple-ios` 的 minimum deployment target 是 **10.0**。
即使传 `IPHONEOS_DEPLOYMENT_TARGET=6.0`，也只会得到一句警告，然后被 clamp 回 10.0：

```
warning: deployment target in IPHONEOS_DEPLOYMENT_TARGET was set to 6.0,
         but the minimum supported by `rustc` is 10.0
clang: warning: overriding '-miphoneos-version-min=6.0' option with
        '-target armv7s-apple-ios10.0.0'
```

**后果**：`-Z build-std` 编出来的 std 是按 **iOS 10 的 API 面**编译的，
会引用 iOS 6/7 根本没有的符号。这不是链接参数能解决的，是 std 的代码路径问题。

### 2. `armv7` 已被 rustc 移除，且 ld64 不认 `-arch armv7`

- rustc 的 `add_apple_link_args()` 里有一张 ld64 架构白名单，
  只有 `armv7k / armv7s / arm64 / arm64e / arm64_32 / i386 / x86_64`，
  **没有 `armv7`**。把 `llvm-target` 改成 `armv7-apple-ios` 会直接触发
  `bug!("unsupported architecture in Apple target")` —— 内部编译器错误（ICE）。
- 所以 `llvm-target` **必须保留 `armv7s-apple-ios`**，链接也必须用 `-arch armv7s`。
- 代价：产物是 **V7S** 薄二进制，只在 A5+（iPhone 4S / iPad 2 / touch 5 及更新）
  上加载；A4（iPhone 3GS/4）的 dyld 不会加载 V7S。

### 3. 链接用的是 Xcode 16.4 的 ld64，但 SDK 是 iOS 6.1

老 SDK + 新链接器有一堆兼容问题，脚本里逐条绕过了（见「坑」表）。

## 关键技术决策

| 决策 | 原因 |
|---|---|
| 自备 `iPhoneOS6.1.sdk` | runner 自带 iOS SDK 是 17/18，全 arm64-only，没有 armv7 桩 |
| `llvm-target` 保持 `armv7s-apple-ios` | 否则触发 rustc 的 arch 白名单 `bug!`（ICE） |
| `cpu = "generic"` + 显式 `features` | 让代码生成是通用 ARMv7（不含 A6 专属指令），配合 `-arch armv7s` 输出 |
| `os = "ios"`（不再是 `macos`） | 链接语义正确：rustc 走 `-target` 而不是 `-mmacosx-version-min` |
| vendored `libc` 0.2.191 + `[patch.crates-io]` | libc 把 `sockaddr_ctl`/`AF_SYS_CONTROL`/`SYSPROTO_CONTROL`/`UTUN_OPT_IFNAME`/`ctl_info`/`CTLIOCGINFO` 全挡在 `cfg(target_os="macos")` 后，`nix`/`tun` 在 iOS 分支要用 |
| `linker-flavor = darwin-cc`（保留） | rustc 校验「flavor 必须与 is_like_darwin 一致」，而 `is_like_darwin` 由 `os` 派生，改不了 |
| `src/shim.rs` | 补 std 引用但老系统缺的 `clock_gettime` / `CCRandomGenerateBytes` |
| pre-link-args 里 `-Wl,-U,<sym>` | 放行 iOS 8 原生有、但 6.1 SDK 的 tbd 没列出的符号（TLS、`*at` 系列） |

## 踩过的坑（真实报错 → 原因 → 解法）

| 报错 | 原因 | 解法 |
|---|---|---|
| `error loading target specification: linker_flavor must be darwin if and only if is_like_darwin is set` | 试过把 flavor 改成 `gnu-cc` 绕过 ICE；但 `is_like_darwin` 由 `os` 派生，`os="macos"/"ios"` 时只能配 darwin flavor | 放弃 gnu-cc 路线，改回 `darwin-cc`，靠 `llvm-target=armv7s` 避开 ICE |
| `error loading target specification: is_like_darwin: unknown field` | JSON target spec 里没有这个字段（它由 `os` 派生） | 不要写它 |
| `cannot find value AF_SYS_CONTROL / sockaddr_ctl / ... in libc` | libc 只对 macos 暴露 utun 系列 | vendored libc + 把 apple 模块的 `cfg(target_os="macos")` 全部放开到 `any(macos, ios)`（`patch_libc_ios.py`） |
| `ld: library not found for -lc` / `-lm` | std/C 依赖会传 `-lc` `-lm` `-lpthread` `-ldl`，iOS 6.1 SDK 没有这些名字的别名 | 用 **iOS 8.1 SDK** 即可（自带 `libc.tbd`/`libm.tbd`）；6.1 时才需手工复制 `libSystem.tbd` 等 |
| `ld: cannot link directly with .../libm.tbd. Link against the umbrella framework` | 复制来的 tbd 带 `parent-umbrella: System`，ld64 拒绝直接 `-l` | 复制后 `sed` 删掉 `parent-umbrella:` 行 |
| `ld: malformed file ... tbd:3:24: error: unknown platform` | 老 tbd 是 `tapi-tbd-v3`，`platform:` 写成 `(null)` | `fix_sdk_tbd.py` 批量改成 `platform: ios`；8.1 SDK 无需 |
| `archive member 'lladdr.o' ... is not mach-o ... for architecture armv7s` | C 依赖用 `-arch armv7` 编，而链接目标是 armv7s | C 依赖统一用 `-arch armv7s` 编 |
| `ld: symbol(s) not found ... _ring_core_*__sha256_block_data_order_nohw`（17 个） | **ring 0.17 的 `ASM_TARGETS` 里 32 位 arm 只配了 Linux/Horizon，没有 Apple** → `asm_target=None`，汇编完全不编译 | `patch_ring_asm.py` 补 `arm + APPLE_ABI + "ios32"` |
| 同上，补了 asm_target 之后仍缺 | **perl 现场生成的 `.S` 没有 `#include <ring-core/asm_base.h>`**，而符号前缀宏 `ring_core_x_y_z__` 就藏在 `asm_base.h → prefix_symbols_asm.h` 里 → 汇编符号没前缀 | `patch_ring_asm.py` 在生成的 `.S` 头部插入该 include |
| `clang: error: unknown argument: '-platform_version'` | `-platform_version` 是 ld64 的参数，不能直接传给 clang 驱动 | 改用 clang 认的 `-miphoneos-version-min=` |
| `ld: symbol(s) not found ... _clock_gettime` | iOS 10 才有 | `src/shim.rs` 用 `gettimeofday` 实现 |
| `ld: symbol(s) not found ... __tlv_bootstrap / _openat ...` | std 按 iOS 10 编；但 iOS 8.1 SDK 里这些符号是**有的** | 用 8.1 SDK 后自动消失；用 6.1 SDK 时需 `-Wl,-U` 放行 |

## 用法

### 云端（GitHub Actions）

1. `workflow-build-ios6.yml` → `.github/workflows/build-ios6.yml`
2. SDK 放 `sdk/iPhoneOS6.1.sdk.tar.gz`（或配 secret `IOS6_SDK_URL`）
3. Actions → `build-ios6` → Run workflow

结果会提交回 `ci-out/`：`status.txt`、`build.log`、成功时还有
`astral-tund.gz` + `astral-tund.sha256` + `artifact-info.txt`。

### 本地 Mac

```bash
mkdir -p work/ios6/sdk && cp -R /path/to/iPhoneOS6.1.sdk work/ios6/sdk/
chmod +x work/ios6/build-ci.sh
REPO_ROOT=$PWD ./work/ios6/build-ci.sh
```

## 脚本清单

| 文件 | 作用 |
|---|---|
| `build-ci.sh` | 主构建脚本：定位/校验/修补 SDK → 生成 target JSON → 编译 → 修版本 → 签名 |
| `patch_crate_types.py` | 把依赖里声明 `cdylib`/`staticlib` 的 crate 收敛成 `rlib`（自定义 target 上会 ICE） |
| `patch_libc_ios.py` | 把 vendored libc 的 apple 模块里 macos-only 的 cfg 放开给 iOS |
| `fix_sdk_tbd.py` | 批量修旧 tbd 的 `platform: (null)` → `ios` |
| `fix_macho_ver.py` | 把产物 Mach-O 的最低版本改成 `$MIN_IOS`（必须在 `ldid` 之前跑） |

## 能否压到 iOS 6

需要额外做的事（按难度排序）：

1. **`clock_gettime`** — 已实现（`src/shim.rs`），复用即可。
2. **`openat`/`fstatat`/`renameat`/`unlinkat`** — iOS 8 才有。iOS 6 要用
   `open`/`fstat`/`rename`/`unlink` + 目录 fd 拼路径来模拟。
3. **线程局部存储（`__tlv_bootstrap`/`__tlv_atexit`）** — iOS 8 才有，**这条最难**。
   `tokio` 和 std 的 `thread_local!` 都靠它。可行方向：
   - target spec 里设 `has-thread-local: false`，让 std 改用 pthread_key 实现；
   - 但这只覆盖 std 自己的 TLS，LLVM 为普通 `#[thread_local]` 生成的访问仍会
     调用 `__tlv_bootstrap`，需要在 C/Rust 里提供一个用 `pthread_key`
     模拟的实现（每个线程分配一块覆盖整个 TLS 段的内存）。
   这条路能走通，但多线程下要非常小心，且需要真机长时间验证。
4. **`CCRandomGenerateBytes`** — 已用 `arc4random_buf` 替代（iOS 2+ 就有），复用即可。

所以「iOS 6」不是不可能，而是要多做第 3 条这个高风险项。
