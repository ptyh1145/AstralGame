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
SPEC="$TARGET_DIR/armv7-ios6.json"
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
lt = spec.get("llvm-target", "armv7s-apple-ios")
spec["llvm-target"] = lt.replace("armv7s", "armv7")
for k in ("llvm-abiname",):
    if k in spec and isinstance(spec[k], str):
        spec[k] = spec[k].replace("armv7s", "armv7")

def patch_args(args):
    """把 -arch armv7s 改成 armv7，并确保带上 -isysroot。"""
    out, i = [], 0
    while i < len(args):
        a = args[i]
        if a == "-arch" and i + 1 < len(args):
            out += ["-arch", "armv7"]; i += 2; continue
        if a == "armv7s":
            out.append("armv7"); i += 1; continue
        out.append(a); i += 1
    if "-isysroot" not in out:
        out += ["-isysroot", sdk]
    return out

for key in ("pre-link-args", "post-link-args"):
    block = spec.get(key)
    if isinstance(block, dict):
        for flavour in list(block):
            block[flavour] = patch_args(list(block[flavour]))
        spec[key] = block
if not spec.get("pre-link-args"):
    spec["pre-link-args"] = {"cc": patch_args(["-arch", "armv7"])}

json.dump(spec, open(dst, "w"), indent=2, sort_keys=True)
print("    wrote", dst)
print("    arch=%s cpu=%s sdk=%s" % (spec.get("arch"), spec.get("cpu"), sdk))
PY
[ -f "$SPEC" ] || die "target JSON 生成失败"

# ---------------------------------------------------------------- 3. 构建
say "构建 astral-tund (armv7, iOS >= $MIN_IOS)"
cd "$CRATE"

# 宿主侧必须是干净环境：一旦 export SDKROOT，build script 链接就会崩
unset SDKROOT
unset IPHONEOS_DEPLOYMENT_TARGET
unset RUSTFLAGS

# C 依赖（zstd-sys 等）走目标专属变量，只影响目标侧编译
CLANG="$(xcrun --sdk macosx -f clang)"
export CC_armv7_ios6="$CLANG"
export CFLAGS_armv7_ios6="-arch armv7 -miphoneos-version-min=$MIN_IOS -isysroot $SDK_PATH -Qunused-arguments"
export CXX_armv7_ios6="$CLANG"
export CXXFLAGS_armv7_ios6="$CFLAGS_armv7_ios6"

cargo +nightly -Zjson-target-spec -Z build-std=std,panic_abort \
  build --release --target "$SPEC"
RC=$?
if [ $RC -ne 0 ]; then
  die "cargo build 失败 (exit $RC)"
fi

BIN="$CRATE/target/armv7-ios6/release/astral-tund"
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
