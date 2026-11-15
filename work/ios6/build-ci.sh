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

# armv7s = ARMv7 + A6 的 Swift 扩展；A5 及更早不认 → 退回通用 armv7
spec["cpu"] = "generic"
spec["features"] = "+v7,+thumb2,+vfp3,+vfp3d16,+neon"
for k in ("llvm-target", "llvm-abiname"):
    if isinstance(spec.get(k), str):
        spec[k] = spec[k].replace("armv7s", "armv7")

# 新版 rustc 的 linker flavour 白名单（旧的 "cc" 已废）
VALID = {
    "gnu", "gnu-lld", "gnu-cc", "gnu-lld-cc",
    "darwin", "darwin-lld", "darwin-cc", "darwin-lld-cc",
    "wasm-lld", "wasm-lld-cc", "unix", "unix-cc",
    "msvc", "msvc-lld", "em-cc", "bpf", "llbc", "gcc",
    "ld", "ld.lld", "ld64.lld", "lld-link", "wasm-ld", "em",
}

def patch_args(args):
    """-arch armv7s → armv7；缺 -isysroot 就补上（只影响目标侧链接）。"""
    out, i = [], 0
    while i < len(args):
        a = args[i]
        if a == "-arch" and i + 1 < len(args):
            out += ["-arch", "armv7"]
            i += 2
            continue
        if a == "armv7s":
            out.append("armv7")
            i += 1
            continue
        out.append(a)
        i += 1
    if "-isysroot" not in out:
        out += ["-isysroot", sdk]
    return out

pla = spec.get("pre-link-args")
if isinstance(pla, dict) and pla and all(k in VALID for k in pla):
    spec["pre-link-args"] = {k: patch_args(list(v)) for k, v in pla.items()}
else:
    # 原 spec 没有可用的 pre-link-args → 用 darwin-cc（不是 "cc"）
    spec["pre-link-args"] = {
        "darwin-cc": patch_args(["-arch", "armv7",
                                 "-miphoneos-version-min=" + min_ios])
    }

# ---------------------------------------------------------------
# target_os 覆盖：
#   libc 只对 target_os="macos" 暴露 utun 需要的
#   sockaddr_ctl / SYSPROTO_CONTROL / UTUN_OPT_IFNAME / AF_SYS_CONTROL，
#   而 nix（tun crate 依赖）在 ios 分支里照样引用它们 → 直接用 os="ios" 编不过。
#   本 daemon 的核心就是 utun，所以让编译期按 macOS 语义走（Darwin 共用 libSystem），
#   但**链接参数强制 iOS 平台 + 最低 6.0**，产物仍是 iOS 二进制。
#   用 TARGET_OS_OVERRIDE=ios 可退回原始语义。
# ---------------------------------------------------------------
os_override = os.environ.get("TARGET_OS_OVERRIDE", "macos")
spec["os"] = os_override
if os_override == "macos":
    args = ["-arch", "armv7",
            "-platform_version", "ios", min_ios, "6.1",
            "-isysroot", sdk]
    spec["pre-link-args"] = {"darwin-cc": args}
    spec.pop("post-link-args", None)

post = spec.get("post-link-args")
if isinstance(post, dict) and post and not all(k in VALID for k in post):
    spec.pop("post-link-args", None)

json.dump(spec, open(dst, "w"), indent=2, sort_keys=True)
print("    arch=%s cpu=%s" % (spec.get("arch"), spec.get("cpu")))
print("    pre-link-args: %s" % json.dumps(spec["pre-link-args"])[:300])
PY
[ -f "$SPEC" ] || die "target JSON 生成失败"

# ---------------------------------------------------------------- 3. 构建
say "构建 astral-tund (armv7, iOS >= $MIN_IOS)"
cd "$CRATE"

# 宿主侧必须是干净环境：一旦 export SDKROOT，build script 链接就会崩
unset SDKROOT
unset IPHONEOS_DEPLOYMENT_TARGET
unset RUSTFLAGS

# C 依赖（ring / zstd-sys 等）走目标专属变量，只影响目标侧编译。
# CRATE_CC_NO_DEFAULTS=1 很关键：因为目标 os 被设成 macos，
# cc-rs 会自动拼出 --target=armv7-apple-macosx -mmacosx-version-min=15.5，
# 与我们的 -miphoneos-version-min=6.0 直接冲突（clang 报 invalid argument）。
# 关掉它的默认旗标后，一切只由 CFLAGS_<target> 决定。
CLANG="$(xcrun --sdk macosx -f clang)"
export CRATE_CC_NO_DEFAULTS=1
# 关掉 strip：`-C strip=debuginfo` + `-Z embed-metadata=no` 叠加 cdylib
# （easytier-core 声明了 rlib+cdylib）在自定义 target 上会让 rustc
# 在 add_order_independent_options 里 ICE。产物也不急着瘦身。
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_INCREMENTAL=0
export CC_armv7_ios6="$CLANG"
export CFLAGS_armv7_ios6="-O2 -fPIC -arch armv7 -miphoneos-version-min=$MIN_IOS -isysroot $SDK_PATH -Qunused-arguments"
export CXX_armv7_ios6="$CLANG"
export CXXFLAGS_armv7_ios6="$CFLAGS_armv7_ios6"

# --- 先预取依赖，然后修补 easytier-core 的 crate-type -------------------------
# easytier-core 声明了 crate-type = ["rlib","cdylib"]。darwin 上 cdylib 需要
# 生成 install_name，自定义 target 会让 rustc 在 add_order_independent_options
# 里 ICE（内部编译器错误）。我们只需要它的 rlib。
# cargo 不会校验 git checkout 的工作树，所以就地改掉再编即可。
say "预取依赖"
cargo +nightly fetch || warn "cargo fetch 有警告，继续"

say "修补 easytier-core（去掉 cdylib）"
PATCHED=0
for f in "$HOME"/.cargo/git/checkouts/easytier-*/*/easytier-core/Cargo.toml; do
  [ -f "$f" ] || continue
  if grep -q 'cdylib' "$f"; then
    /usr/bin/sed -i '' 's/"rlib", "cdylib"/"rlib"/' "$f"
    if grep -q 'cdylib' "$f"; then
      /usr/bin/sed -i '' 's/"rlib","cdylib"/"rlib"/' "$f"
    fi
    echo "    patched: $f"
    grep -n 'crate-type' "$f" | sed 's/^/      /'
    PATCHED=1
  fi
done
if [ "$PATCHED" = "0" ]; then
  warn "没找到含 cdylib 的 easytier-core/Cargo.toml（路径可能变了）"
else
  echo "    ok: easytier-core 现在只编 rlib"
fi

say "编译"
cargo +nightly -Zjson-target-spec -Z build-std=std,panic_abort \
  build --release --target "$SPEC"
RC=$?if [ $RC -ne 0 ]; then
  die "cargo build 失败 (exit $RC)"
fi

BIN="$CRATE/target/armv7-apple-ios/release/astral-tund"
[ -f "$BIN" ] || BIN="$(find "$CRATE/target" -name astral-tund -type f 2>/dev/null | head -1)"
[ -f "$BIN" ] || die "找不到产物 astral-tund"

# ---------------------------------------------------------------- 4. 后处理
say "校验产物"
file "$BIN" | sed 's/^/    /'
otool -l "$BIN" | grep -A4 LC_VERSION_MIN_IPHONEOS | sed 's/^/    /' || warn "无 LC_VERSION_MIN_IPHONEOS"
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
