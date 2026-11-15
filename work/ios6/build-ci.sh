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
MIN_IOS="${MIN_IOS:-6.0}"

say()  { printf "\033[1;34m==> %s\033[0m\n" "$*"; }
warn() { printf "\033[1;33m!!  %s\033[0m\n" "$*"; }
die()  { printf "\033[1;31mXX  %s\033[0m\n" "$*"; exit 1; }

# ---------------------------------------------------------------- 1. SDK
say "定位 iOS SDK"
SDK_PATH=""
for cand in \
  "$HERE/sdk/iPhoneOS6.1.sdk" \
  "$REPO_ROOT/ios6-sdk/iPhoneOS6.1.sdk" \
  "$REPO_ROOT/sdk/iPhoneOS6.1.sdk"
do
  [ -z "$SDK_PATH" ] && [ -d "$cand" ] && SDK_PATH="$cand"
done
[ -n "$SDK_PATH" ] || die "找不到 iPhoneOS6.1.sdk（见 README；runner 不带）"
echo "    SDK = $SDK_PATH"

say "校验 SDK 真伪"
LIB_SYS="$SDK_PATH/usr/lib/libSystem.tbd"
LIB_M="$SDK_PATH/usr/lib/system/libsystem_m.tbd"
LIB_OBJC="$SDK_PATH/usr/lib/libobjc.tbd"
for f in "$LIB_SYS" "$LIB_M" "$LIB_OBJC"; do
  [ -f "$f" ] || die "SDK 缺少 $f"
done
grep -q 'armv7' "$LIB_SYS" || die "libSystem.tbd 里没有 armv7 —— 不是 32 位 SDK"
echo "    armv7 支持: OK"
grep -q -- '___sincos_stret' "$LIB_M" && warn "含 ___sincos_stret（iOS7+），SDK 偏新" \
  || echo "    iOS 6 特征: 无 ___sincos_stret  OK"
grep -q -- '_objc_readClassPair' "$LIB_OBJC" && warn "含 _objc_readClassPair（iOS8+），SDK 偏新" \
  || echo "    iOS 6 特征: 无 _objc_readClassPair  OK"

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
spec["pre-link-args"] = {
    "darwin-cc": [
        "-arch", "armv7s",
        "-miphoneos-version-min=" + min_ios,
        "-isysroot", sdk
    ]
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
C_CFLAGS="-O2 -fPIC -arch armv7 -miphoneos-version-min=$MIN_IOS -isysroot $SDK_PATH -Qunused-arguments"
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

say "编译"
cargo +nightly -Zjson-target-spec -Z build-std=std,panic_abort \
  build --release --target "$SPEC"
RC=$?
if [ $RC -ne 0 ]; then
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
python3 "$HERE/fix_macho_ver.py" "$BIN" "$MIN_IOS" "6.1" || warn "版本修正失败（继续）"

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
