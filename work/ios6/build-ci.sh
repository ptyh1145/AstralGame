#!/bin/bash
# 在 macOS 上构建 iOS 6 (armv7) 二进制。本地 Mac 与 GitHub Actions 通用。
#
# 关键设计（踩过的坑都在注释里）：
#   1. runner 自带 Xcode 只有 15/16、iOS SDK 17/18（全 arm64-only）→ 必须自备 6.1 SDK。
#   2. 当前 Rust 已移除 armv7-apple-ios，只剩 armv7s-apple-ios（A6 的 Swift 扩展）
#      → 拿 armv7s 的 spec 派生 cpu=generic 的纯 armv7 目标。
#   3. nightly 现在要求 -Zjson-target-spec 才肯吃 .json 目标。
#   4. **绝对不能把 SDKROOT / -isysroot 放进全局环境**：build script 和 proc-macro
#      是给宿主（x86_64-macOS）编的，会被 iPhoneOS 的 sysroot 搞死
#      （症状：`ld: library 'c' not found`）。
#      → SDK 路径只写进 target JSON 的 pre-link-args，宿主侧保持默认。
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$HERE/../.." && pwd)}"
CRATE="${CRATE:-$REPO_ROOT/work/astral-tund}"
OUT="${OUT:-$REPO_ROOT/dist}"
MIN_IOS="${MIN_IOS:-8.0}"

say()  { printf "\033[1;34m==> %s\033[0m\n" "$*"; }
warn() { printf "\033[1;33m!!  %s\033[0m\n" "$*"; }
die()  { printf "\033[1;31mXX  %s\033[0m\n" "$*"; exit 1; }

# ---------------------------------------------------------------- 1. SDK
say "定位 iOS SDK"
# 优先 8.1：rustc 1.102 的 std 是按 iOS 10 的 API 面编的，8.1 SDK 已经提供
# 了 __tlv_bootstrap / openat / *at 系列，能省掉一堆 -Wl,-U 放行；
# 6.1 SDK 作为后备（只有做 iOS 6/7 时才会用到）。
SDK_PATH=""
for cand in \
  "$HERE/sdk/iPhoneOS8.1.sdk" \
  "$REPO_ROOT/ios81-sdk/iPhoneOS8.1.sdk" \
  "$REPO_ROOT/sdk/iPhoneOS8.1.sdk" \
  "$HERE/sdk/iPhoneOS6.1.sdk" \
  "$REPO_ROOT/ios6-sdk/iPhoneOS6.1.sdk" \
  "$REPO_ROOT/sdk/iPhoneOS6.1.sdk"
do
  [ -z "$SDK_PATH" ] && [ -d "$cand" ] && SDK_PATH="$cand"
done
[ -n "$SDK_PATH" ] || die "找不到 iPhoneOS8.1.sdk / iPhoneOS6.1.sdk（见 README；runner 不带）"
echo "    SDK = $SDK_PATH"
SDK_IS_61=0
case "$SDK_PATH" in *iPhoneOS6.1*) SDK_IS_61=1 ;; esac

say "校验 SDK 真伪"
LIB_SYS="$SDK_PATH/usr/lib/libSystem.tbd"
LIB_M="$SDK_PATH/usr/lib/system/libsystem_m.tbd"
LIB_OBJC="$SDK_PATH/usr/lib/libobjc.tbd"
for f in "$LIB_SYS" "$LIB_M" "$LIB_OBJC"; do
  [ -f "$f" ] || die "SDK 缺少 $f"
done
grep -q 'armv7' "$LIB_SYS" || die "libSystem.tbd 里没有 armv7 —— 不是 32 位 SDK"
echo "    armv7 支持: OK"
if [ "$SDK_IS_61" = "1" ]; then
  # 这两条只在用 6.1 SDK 时才有意义（判断它是不是被"加料"过的新 SDK）。
  grep -q -- '___sincos_stret' "$LIB_M" && warn "含 ___sincos_stret（iOS7+），SDK 偏新" \
    || echo "    iOS 6 特征: 无 ___sincos_stret  OK"
  grep -q -- '_objc_readClassPair' "$LIB_OBJC" && warn "含 _objc_readClassPair（iOS8+），SDK 偏新" \
    || echo "    iOS 6 特征: 无 _objc_readClassPair  OK"
else
  echo "    用的是 iOS 8.x SDK（armv7 + TLS + *at 符号齐全），跳过 iOS6 特征检查"
fi

# 下面两步只对 6.1 SDK 有意义，8.1 SDK 自带 libc/libm 别名、platform 字段也合法。
if [ "$SDK_IS_61" = "1" ]; then

# 补经典库名别名（libc / libm / libpthread / libdl）。
# 原因：std（build-std）和各种 C 依赖会传 -lc / -lm / -lpthread / -ldl，
# 但 iOS 把这些符号全都并进 libSystem / libsystem_m 里，这个 6.1 SDK
# 只提供了 libSystem.tbd 和 system/libsystem_m.tbd，没有经典名字的别名
# （真实 Xcode 的 iOS SDK 是有的），于是 ld 报
# `ld: library not found for -lc` / `-lm` ...
# 复制对应 tbd 成别名即可：tbd 里的 install-name 保持不变
# （/usr/lib/libSystem.B.dylib、/usr/lib/system/libsystem_m.dylib），
# ld 只按 install-name 记录 LC_LOAD_DYLIB，语义完全等价，真机都有这些 dylib。
alias_lib() {
  name="$1"; srcfile="$2"
  if [ ! -f "$SDK_PATH/usr/lib/$name.tbd" ]; then
    if [ -f "$srcfile" ]; then
      cp "$srcfile" "$SDK_PATH/usr/lib/$name.tbd"
      # 删掉 parent-umbrella：带这个字段的 tbd 会被 ld64 认为是子库，
      # 拒绝直接 -l 链接（报 "cannot link directly with ... Link against
      # the umbrella framework 'System.framework' instead"）。
      # 真实 Xcode iOS SDK 里的 libc/libm 别名没有这个字段。
      sed -i '' '/^parent-umbrella:/d' "$SDK_PATH/usr/lib/$name.tbd"
      echo "    补 $name.tbd -> $(basename "$srcfile")"
    else
      warn "找不到 $(basename "$srcfile")，无法补 $name.tbd"
    fi
  fi
}
alias_lib libc      "$SDK_PATH/usr/lib/libSystem.tbd"
alias_lib libm      "$SDK_PATH/usr/lib/system/libsystem_m.tbd"
alias_lib libpthread "$SDK_PATH/usr/lib/libSystem.tbd"
alias_lib libdl     "$SDK_PATH/usr/lib/libSystem.tbd"

# 修补旧式 tbd 的 platform 字段。
# 这批 6.1 SDK 的 tbd 是 tapi-tbd-v3，platform 写成 `(null)`，
# Xcode 16 的 ld64 直接报 `error: unknown platform`（连 libSystem 都不放过）。
# 把 v3 tbd 的 `platform: (null)` 统一改成 `platform: ios`。
say "修补 SDK 的 tbd platform 字段（(null) -> ios）"
python3 "$HERE/fix_sdk_tbd.py" "$SDK_PATH" || warn "tbd 修补失败（继续）"

fi  # SDK_IS_61

# ---------------------------------------------------------------- 2. target JSON
say "派生纯 armv7 目标（基于 armv7s spec，注入 SDK 链接参数）"
export RUSTC_BOOTSTRAP=1
TARGET_DIR="${TARGET_DIR:-$HERE/targets}"
mkdir -p "$TARGET_DIR"
# 目标名必须是三段式：有些 crate 的 build.rs 会用
# `TARGET.split('-').nth(2)` 粗解析平台名（如 sys-info），
# 两段式（armv7-ios6）会让它 unwrap 到 None 直接 panic。
SPEC="$TARGET_DIR/armv7-apple-ios.json"
TMP_SPEC="$TARGET_DIR/_armv7s.json"

rustc -Z unstable-options --print target-spec-json --target armv7s-apple-ios > "$TMP_SPEC" \
  || die "拿不到 armv7s 的 target spec（nightly 是否已装？）"

SDK_PATH="$SDK_PATH" python3 - "$TMP_SPEC" "$SPEC" "$MIN_IOS" <<'PY'
import json, os, sys

src, dst, min_ios = sys.argv[1], sys.argv[2], sys.argv[3]
sdk = os.environ["SDK_PATH"]
spec = json.load(open(src))

# --- 代码生成：通用 armv7（兼容 A4/A5，无 sctl NEON）---
# cpu=generic + 显式 features 确保不生成 A6-only 指令。
# llvm-target 保持 armv7s-apple-ios 不改：
#   代码生成由 cpu+features 完全控制（LLVM setCPU("generic") 后
#   显式 features 决定最终指令集），triple 的 arch 部分不影响。
#   保留 armv7s 是因为 rustc 的 add_apple_link_args 会把 llvm_arch
#   拿去 match（白名单只有 armv7k/armv7s/arm64...，armv7 不在 → bug!）。
spec["cpu"] = "generic"
spec["features"] = "+v7,+thumb2,+vfp3,+vfp3d16,+neon"

# --- os：iOS（真实语义）---
# 之前用 os="macos" 是因为 libc 把 utun 的 6 个符号全挡在 macos-only。
# 现在用 vendored libc（work/vendor/libc-0.2.191）+ [patch.crates-io]，
# 把 sockaddr_ctl / AF_SYS_CONTROL / SYSPROTO_CONTROL / UTUN_OPT_IFNAME /
# ctl_info / CTLIOCGINFO 放开给 target_os="ios"，所以可以放心用真 iOS 语义。
# 注意：这样 rustc 的 darwin flavor 会走 iOS 分支（-target 而非 -mmacosx-version-min），
# 不再有 macos 版 flag 冲突。
spec["os"] = "ios"
# linker-flavor 保持派生 spec 自带的 "darwin-cc"（is_like_darwin=true 的合法 flavor）。
# 只要 os=ios 且 llvm-target 是 armv7s，add_apple_link_args 就不触发 bug!。
spec.pop("is_like_darwin", None)  # 该字段不存在于 JSON，误加会报错

# --- pre-link-args：Apple 平台参数 ---
# -arch armv7s：ld64 白名单里的 arch（armv7 不在）。
#   ⚠ 注意：V7S 产物只在 A5+（iPhone 4S/iPad 2+）上跑；A4（iPhone 3GS/4）
#   不会加载 V7S 二进制。这是现代 ld64 已移除 armv7 的代价。
# -miphoneos-version-min=<min>：clang 级别的 flag，明确 iOS 平台 + 最低版本。
#   注意 **不能** 用 -platform_version：那是 ld64 的 flag，clang 驱动不认
#   （报 "unknown argument: '-platform_version'"，且会把后面的 ios/6.0 当文件）。
# -isysroot <6.1 SDK>：链接器找库/headers 用我们的 iOS 6.1 SDK
#
# 下面的 -Wl,-U,<sym>：放行"系统里有、但链接期解析不到"的符号。
#
# __tlv_bootstrap / __tlv_atexit：线程局部存储（std 的 thread_local!、
#   tokio 的 runtime context 都用它）。这两个符号在 **libdyld** 里，
#   而我们的链接命令行只有 -lc/-lm 等（libc.tbd 不含 dyld 的符号），
#   所以哪怕是 iOS 8.1 SDK 也会报 undefined。iOS 8+ 的 dyld 原生提供它们，
#   放行是安全的。
# openat / fstatat / renameat / unlinkat：只在 6.1 SDK 上需要放行
#   （8.1 SDK 的 libsystem_kernel.tbd 里已经有了）。
extra_link = [
    "-Wl,-U,__tlv_bootstrap",
    "-Wl,-U,__tlv_atexit",
]
if "iPhoneOS6.1" in sdk:
    extra_link += [
        "-Wl,-U,_openat",
        "-Wl,-U,_fstatat",
        "-Wl,-U,_renameat",
        "-Wl,-U,_unlinkat",
    ]
# 另外两个由 src/shim.rs 自己实现（不依赖额外库）：
#   clock_gettime         → iOS 10 才有的，任何 8.x/9.x SDK 都没有
#   CCRandomGenerateBytes → 用 libSystem 的 arc4random_buf 替代
spec["pre-link-args"] = {
    "darwin-cc": [
        "-arch", "armv7s",
        "-miphoneos-version-min=" + min_ios,
        "-isysroot", sdk,
    ] + extra_link
}
spec.pop("post-link-args", None)

json.dump(spec, open(dst, "w"), indent=2, sort_keys=True)
print("    arch=%s cpu=%s" % (spec.get("arch"), spec.get("cpu")))
print("    llvm-target=%s (保留 armv7s, 代码生成由 cpu+features 控制)" % spec.get("llvm-target"))
print("    linker-flavor=%s" % spec.get("linker-flavor"))
print("    os=%s" % spec.get("os"))
print("    pre-link-args: %s" % json.dumps(spec["pre-link-args"])[:300])
PY
[ -f "$SPEC" ] || die "target JSON 生成失败"

# ---------------------------------------------------------------- 3. 构建
say "构建 astral-tund (armv7, iOS >= $MIN_IOS)"
cd "$CRATE"

# 宿主侧必须是干净环境：一旦 export SDKROOT，build script 链接就会崩
unset SDKROOT
unset RUSTFLAGS
# IPHONEOS_DEPLOYMENT_TARGET 控制 rustc 的 apple_deployment_target()，
# 让 versioned_llvm_target 变成 armv7s-apple-ios6.0（即 clang 的 -target）。
# 宿主（macOS）rustc 只读 MACOSX_DEPLOYMENT_TARGET，不受此影响。
export IPHONEOS_DEPLOYMENT_TARGET="$MIN_IOS"

# C 依赖（ring / zstd-sys 等）走目标专属变量，只影响目标侧编译。
# CRATE_CC_NO_DEFAULTS=1：不依赖 cc-rs 自动推导平台 flag，
# 一切只由 CFLAGS_<target> 决定，避免 cc-rs 用宿主推断出冲突的版本。
CLANG="$(xcrun --sdk macosx -f clang)"
export CRATE_CC_NO_DEFAULTS=1
# 关掉 strip：`-C strip=debuginfo` + `-Z embed-metadata=no` 叠加 cdylib
# （easytier-core 声明了 rlib+cdylib）在自定义 target 上会让 rustc
# 在 add_order_independent_options 里 ICE。产物也不急着瘦身。
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_INCREMENTAL=0
# cc-rs 的 env 变量名基于 TARGET triple（可能解析成多种形态），
# 保险起见把 armv7s/armv7 × apple-ios/ios 的组合都设上同一组 CFLAGS。
# 注意 -arch 必须是 **armv7s**：最终链接用 -arch armv7s（ld64 已不支持
# 纯 armv7），若 C 目标文件编成 armv7，ld64 会报
# "archive member 'xxx.o' ... is not mach-o ... for architecture armv7s"。
C_CFLAGS="-O2 -fPIC -arch armv7s -miphoneos-version-min=$MIN_IOS -isysroot $SDK_PATH -Qunused-arguments"
export CC_armv7s_apple_ios="$CLANG";   export CFLAGS_armv7s_apple_ios="$C_CFLAGS"
export CC_armv7s_ios="$CLANG";         export CFLAGS_armv7s_ios="$C_CFLAGS"
export CC_armv7_apple_ios="$CLANG";    export CFLAGS_armv7_apple_ios="$C_CFLAGS"
export CC_armv7_ios="$CLANG";          export CFLAGS_armv7_ios="$C_CFLAGS"
export CXX_armv7s_apple_ios="$CLANG";  export CXXFLAGS_armv7s_apple_ios="$C_CFLAGS"
export CXX_armv7s_ios="$CLANG";        export CXXFLAGS_armv7s_ios="$C_CFLAGS"
export CXX_armv7_apple_ios="$CLANG";   export CXXFLAGS_armv7_apple_ios="$C_CFLAGS"
export CXX_armv7_ios="$CLANG";         export CXXFLAGS_armv7_ios="$C_CFLAGS"

# --- 先预取依赖，然后修补 easytier-core 的 crate-type -------------------------
# easytier-core 声明了 crate-type = ["rlib","cdylib"]。darwin 上 cdylib 需要
# 生成 install_name，自定义 target 会让 rustc 在 add_order_independent_options
# 里 ICE（内部编译器错误）。我们只需要它的 rlib。
# cargo 不会校验 git checkout 的工作树，所以就地改掉再编即可。
say "预取依赖"
cargo +nightly fetch || warn "cargo fetch 有警告，继续"

# 只要某个依赖声明了 crate-type 含 cdylib / staticlib，cargo 就会给那次编译
# 加上 --crate-type cdylib/staticlib，rustc 在自定义 target 上会在
# add_order_independent_options 里 ICE（easytier-core、boringtun 都踩过）。
# 我们只需要把 rlib 链进最终二进制，所以统一收敛成 ["rlib"]。
# cargo 不校验 registry/git 源码目录的内容，改完直接编即可。
say "修补依赖的 crate-type（cdylib/staticlib → rlib）"
# 用 python 做多行安全替换：boringtun 的 crate-type 是跨多行数组，
# 单行 sed 正则匹配不到。cargo 不校验源码内容，改完直接编即可。
python3 "$HERE/patch_crate_types.py" \
  "$HOME/.cargo/registry/src" \
  "$HOME/.cargo/git/checkouts" || warn "crate-type 修补脚本失败（继续尝试编译）"

# ring 的 32 位 iOS 汇编问题已经**预先在 vendored 副本里修好**了
# （work/vendor/ring-0.17.14：加 arm + APPLE_ABI + "ios32" 汇编目标、
#  预生成 7 个 *_ios32.S、iOS 下改用 poly1305 的通用 C 实现），
# 通过 [patch.crates-io] 以 path 依赖接入，所以这里不需要再运行时打补丁。
# 之前"改 registry 源码"的方式始终不生效：cargo 的 .fingerprint 认定了
# 旧的 build script，补丁永远没被跑过（日志里连 build/ring 目录都没有）。
say "ring 使用 vendored 副本（已内置 iOS 32 位汇编修复）"

# 关键：强制 ring 重新跑 build script 并重新生成汇编。
#
# 之前几轮一直失败的原因（很隐蔽）：
#   **build script 是给 host 编译并运行的**，所以它的产物在
#   `target/release/build/ring-<hash>/`（没有 triple 那一层），
#   而不是 `target/<triple>/release/build/`。
#   我们一直在清后者，等于没清到 build script 的缓存，
#   cargo 就始终复用"已成功执行"的旧状态（out/ 空、libring_core*.a 不存在），
#   最终表现为 17 个 _ring_core_* 符号 undefined。
#
# 所以这里必须清理 **host 侧** 的 ring 构建状态。
say "强制 ring 重编（清 host 侧 build script 缓存）"
cargo +nightly clean -p ring || warn "cargo clean -p ring 失败（继续）"
echo "    cargo clean -p ring 完成"

for p in "$CRATE"/target/release/build/ring-* \
         "$CRATE"/target/release/.fingerprint/ring-* \
         "$CRATE"/target/armv7-apple-ios/release/build/ring-* \
         "$CRATE"/target/armv7-apple-ios/release/.fingerprint/ring-* \
         "$CRATE"/target/armv7-apple-ios/release/deps/libring-* ; do
  [ -e "$p" ] && rm -rf "$p"
done
echo "    兜底清理完成"

# C 依赖的 build 输出也清掉：rust-cache 里可能存着用旧 -arch armv7 编的
# 目标文件，ld64 遇到架构不符的归档成员会**静默丢弃**，最后表现成
#   ld: symbol(s) not found ... _ring_core_*_sha512_block_data_order_nohw
BUILD_DIR="$CRATE/target/armv7-apple-ios/release/build"
if [ -d "$BUILD_DIR" ]; then
  rm -rf "$BUILD_DIR"
  echo "    已清理 target/.../build（强制 C 依赖按 armv7s 重编）"
fi

say "编译"
cargo +nightly -Zjson-target-spec -Z build-std=std,panic_abort \
  build --release --target "$SPEC"
RC=$?
if [ $RC -ne 0 ]; then
  # 失败时把 ring 的中间产物打印出来（排 ring 汇编问题用，只读不写）
  say "构建失败 —— dump ring 汇编诊断"
  RINGOUT=""
  # build script 在 host 侧：target/release/build/ring-<hash>
  for d in "$CRATE"/target/release/build/ring-*; do
    [ -d "$d" ] && RINGOUT="$d" && break
  done
  if [ -n "$RINGOUT" ]; then
    echo "    ring build dir: $RINGOUT"
    echo "    --- $RINGOUT 目录内容 ---"
    ls -la "$RINGOUT" 2>/dev/null | head -12 | sed 's/^/      /'
    echo "    --- $RINGOUT/out 目录内容 ---"
    ls -la "$RINGOUT/out" 2>/dev/null | head -20 | sed 's/^/      /'
    echo "    --- build script 输出（尾部 40 行）---"
    tail -40 "$RINGOUT/output" 2>/dev/null | sed 's/^/      /'
    echo "    --- out/ 下的 .S 文件 ---"
    find "$RINGOUT/out" -name '*.S' 2>/dev/null | head -20 | sed 's/^/      /'
    echo "    --- out/ 下的 .o 文件 ---"
    find "$RINGOUT/out" -name '*.o' 2>/dev/null | head -30 | sed 's/^/      /'
    echo "    --- ring_core_generated 目录 ---"
    ls "$RINGOUT/out/ring_core_generated" 2>/dev/null | head -10 | sed 's/^/      /'
    ARFILE=$(find "$RINGOUT/out" -name 'libring_core*.a' 2>/dev/null | head -1)
    if [ -n "$ARFILE" ]; then
      echo "    --- $ARFILE 里的 sha256/vpaes 符号 ---"
      nm -g "$ARFILE" 2>/dev/null | grep -i 'sha256_block\|vpaes_set\|poly1305_neon' | head -20 | sed 's/^/      /'
      echo "    --- 归档成员 ---"
      ar t "$ARFILE" 2>/dev/null | head -20 | sed 's/^/      /'
    else
      echo "    !! 没找到 libring_core*.a"
    fi
  else
    echo "    !! 没找到 ring 的 build 目录"
  fi
  die "cargo build 失败 (exit $RC)"
fi

BIN="$CRATE/target/armv7-apple-ios/release/astral-tund"
[ -f "$BIN" ] || BIN="$(find "$CRATE/target" -name astral-tund -type f 2>/dev/null | head -1)"
[ -f "$BIN" ] || die "找不到产物 astral-tund"

# ---------------------------------------------------------------- 4. 后处理
# rustc 1.102 对 armv7s 的 minimum deployment target 是 10.0，它会强制
# `-target armv7s-apple-ios10.0.0`，覆盖我们给 clang 的 -miphoneos-version-min=6.0，
# 产物会被标成 min=10.0。这里改回 6.0（必须在 ldid 签名之前）。
say "修正 Mach-O 最低版本 -> iOS $MIN_IOS"
python3 "$HERE/fix_macho_ver.py" "$BIN" "$MIN_IOS" "$MIN_IOS" || warn "版本修正失败（继续）"

say "校验产物"
file "$BIN" | sed 's/^/    /'
otool -l "$BIN" | grep -A4 -E 'LC_VERSION_MIN_IPHONEOS|LC_BUILD_VERSION' | sed 's/^/    /' || warn "无 iOS 版本命令"
otool -L "$BIN" | head -12 | sed 's/^/    /'

say "签名（越狱设备 ad-hoc）"
if command -v ldid >/dev/null; then
  ldid -S "$BIN" && echo "    ldid -S 完成"
else
  warn "没有 ldid，跳过签名"
fi

mkdir -p "$OUT"
cp "$BIN" "$OUT/astral-tund"
( cd "$OUT" && shasum -a 256 astral-tund > astral-tund.sha256 )
say "产物: $OUT/astral-tund"
cat "$OUT/astral-tund.sha256"

# ---------------------------------------------------------------- 5. GUI (.app / .ipa)
# 用原生 UIKit 界面 + 上面的 Rust 静态库，打包成越狱设备可装的 .ipa。
# （原版 Flutter 界面跑不了：Flutter 引擎只有 arm64、最低 iOS 13）
say "编译原生界面并打包 .ipa"
UI_DIR="$REPO_ROOT/work/ios-ui"
LIB="$CRATE/target/armv7-apple-ios/release/libastral_tund.a"
if [ ! -f "$LIB" ]; then
  warn "找不到 libastral_tund.a（[lib] crate-type 是否含 staticlib？）—— 跳过 .ipa"
else
  APP="$OUT/AstralGame.app"
  rm -rf "$APP" "$OUT/Payload" "$OUT/main.o"
  mkdir -p "$APP"

  UI_CFLAGS=(-arch armv7s "-miphoneos-version-min=$MIN_IOS" -isysroot "$SDK_PATH"
             -O2 -fobjc-arc -Wno-deprecated-declarations -Wno-unused-parameter)

  echo "    [1/4] 编译 main.m"
  clang "${UI_CFLAGS[@]}" -c "$UI_DIR/main.m" -o "$OUT/main.o" \
    || die "ObjC 编译失败"

  echo "    [2/4] 链接 App（含 Rust 静态库）"
  clang "${UI_CFLAGS[@]}" \
    "$OUT/main.o" \
    -Wl,-force_load,"$LIB" \
    -framework UIKit -framework Foundation -framework CoreGraphics \
    -framework CoreFoundation -framework SystemConfiguration -framework Security \
    -lc -lm -liconv -lresolv \
    -Wl,-U,__tlv_bootstrap -Wl,-U,__tlv_atexit \
    -o "$APP/AstralGame" \
    || die "链接 App 失败"

  echo "    [3/4] 修正版本 + 签名"
  python3 "$HERE/fix_macho_ver.py" "$APP/AstralGame" "$MIN_IOS" "$MIN_IOS" \
    || warn "版本修正失败（继续）"
  if command -v ldid >/dev/null; then
    ldid -S "$APP/AstralGame" && echo "        ldid -S 完成"
  else
    warn "没有 ldid，App 未签名"
  fi
  cp "$UI_DIR/Info.plist" "$APP/"

  echo "    [4/4] 打包 ipa"
  mkdir -p "$OUT/Payload"
  cp -R "$APP" "$OUT/Payload/"
  rm -f "$OUT/AstralGame.ipa"
  ( cd "$OUT" && zip -qr AstralGame.ipa Payload )
  ( cd "$OUT" && shasum -a 256 AstralGame.ipa > AstralGame.ipa.sha256 )
  say "GUI 产物: $OUT/AstralGame.ipa"
  cat "$OUT/AstralGame.ipa.sha256"
  file "$APP/AstralGame" | sed 's/^/    /'
  otool -l "$APP/AstralGame" | grep -A4 -E 'LC_VERSION_MIN_IPHONEOS|LC_BUILD_VERSION' | sed 's/^/    /' || true
  otool -L "$APP/AstralGame" | head -12 | sed 's/^/    /'
fi
