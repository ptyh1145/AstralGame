#!/bin/bash
# 在 GitHub Actions 的 macOS runner 上构建 iOS 6 (armv7) 二进制。
# 也可在本地 Mac 上直接跑。
#
# 关键背景（为什么不能直接用 runner 自带工具链）：
#   1. runner 的 Xcode 只有 15/16，iOS SDK 只有 17/18 —— 全是 arm64-only，
#      没有 armv7 的 libSystem 桩，链接必失败。
#   2. 当前 Rust 已移除 `armv7-apple-ios`（只剩 `armv7s-apple-ios`，Tier 3）。
#
# 因此本脚本做三件事：
#   A. 指向自备的 iPhoneOS6.1.sdk
#   B. 以 `armv7s-apple-ios` 的 target spec 为模板，派生出 cpu=generic 的
#      纯 armv7 目标 JSON（armv7s 多出的 Swift 扩展对 A5 设备不兼容）
#   C. nightly + `-Z build-std`（Tier 3 目标没有预编译 std）
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$HERE/../.." && pwd)}"
CRATE="${CRATE:-$REPO_ROOT/work/astral-tund}"
OUT="${OUT:-$REPO_ROOT/dist}"
MIN_IOS="${MIN_IOS:-6.0}"

say()  { printf "\033[1;34m==> %s\033[0m\n" "$*"; }
warn() { printf "\033[1;33m!!  %s\033[0m\n" "$*"; }
die()  { printf "\033[1;31mXX  %s\033[0m\n" "$*"; exit 1; }

# ---------------------------------------------------------------- A. SDK
say "定位 iOS SDK"
SDKROOT="${SDKROOT:-}"
for cand in \
  "$HERE/sdk/iPhoneOS6.1.sdk" \
  "$REPO_ROOT/ios6-sdk/iPhoneOS6.1.sdk" \
  "$REPO_ROOT/sdk/iPhoneOS6.1.sdk"
do
  [ -z "$SDKROOT" ] && [ -d "$cand" ] && SDKROOT="$cand"
done

[ -n "$SDKROOT" ] || die "找不到 iPhoneOS6.1.sdk —— 见 README.md（必须自备，runner 不带）"
[ -d "$SDKROOT" ] || die "SDKROOT 不存在: $SDKROOT"
export SDKROOT
echo "    SDKROOT = $SDKROOT"

# ---------------------------------------------------------------
# SDK 真伪校验：确认它既支持 armv7，又真的是 iOS 6 的导出表。
# 为什么要查符号：现代 .tbd 会包含 iOS 7+ 才有的函数，链接照样过，
# 真机一跑就 "Symbol not found"。iOS 6 的 SDK 里必须**没有**这些符号。
# ---------------------------------------------------------------
say "校验 SDK 真伪"
LIB_SYS="$SDKROOT/usr/lib/libSystem.tbd"
LIB_M="$SDKROOT/usr/lib/system/libsystem_m.tbd"
LIB_OBJC="$SDKROOT/usr/lib/libobjc.tbd"
LIB_DISPATCH="$SDKROOT/usr/lib/system/libdispatch.tbd"

for f in "$LIB_SYS" "$LIB_M" "$LIB_OBJC"; do
  [ -f "$f" ] || die "SDK 缺少 $f（不是完整 SDK）"
done

grep -q 'armv7' "$LIB_SYS" || die "libSystem.tbd 里没有 armv7 —— 这不是 32 位 SDK"
echo "    armv7 支持: OK"

if grep -q -- '___sincos_stret' "$LIB_M" 2>/dev/null; then
  warn "libsystem_m.tbd 含 ___sincos_stret（iOS 7+ 符号）—— 这份 SDK 偏新，运行期可能缺符号"
else
  echo "    iOS 6 特征: libsystem_m 无 ___sincos_stret  OK"
fi

if grep -q -- '_objc_readClassPair' "$LIB_OBJC" 2>/dev/null; then
  warn "libobjc.tbd 含 _objc_readClassPair（iOS 8+ 符号）—— SDK 偏新"
else
  echo "    iOS 6 特征: libobjc 无 _objc_readClassPair  OK"
fi

if [ -f "$LIB_DISPATCH" ] && grep -q -- '_dispatch_activate' "$LIB_DISPATCH" 2>/dev/null; then
  warn "libdispatch.tbd 含 _dispatch_activate（iOS 10+ 符号）—— SDK 偏新"
fi

if command -v plutil >/dev/null; then
  plutil -extract CanonicalName raw "$SDKROOT/SDKSettings.plist" 2>/dev/null \
    | sed 's/^/    CanonicalName = /' || true
fi

# ---------------------------------------------------------------- B. 目标 JSON
say "派生纯 armv7 目标（基于 armv7s 的 spec）"
export RUSTC_BOOTSTRAP=1
TARGET_DIR="${TARGET_DIR:-$HERE/targets}"
mkdir -p "$TARGET_DIR"
SPEC="$TARGET_DIR/armv7-ios6.json"

rustc -Z unstable-options --print target-spec-json --target armv7s-apple-ios > "$TARGET_DIR/_armv7s.json"

python3 - "$TARGET_DIR/_armv7s.json" "$SPEC" <<'PY'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
d = json.load(open(src))
# armv7s = ARMv7 + Swift 扩展（A6 专有）；A5 及其它 armv7 设备不认。
# 改成 generic armv7 + 标准 NEON/VFPv3，即可在 iPhone 4S / iPad 2/3 / touch5 上跑。
d["cpu"] = "generic"
d["features"] = "+v7,+thumb2,+vfp3,+vfp3d16,+neon"
d["llvm-target"] = d.get("llvm-target", "armv7-apple-ios").replace("7s", "7")
# 让产物能直接跑在 iOS 6 上
d["post-link-args"] = d.get("post-link-args", {})
json.dump(d, open(dst, "w"), indent=2, sort_keys=True)
print("    wrote", dst)
PY

say "检查目标文件"
python3 -c "import json;d=json.load(open('$SPEC'));print('    cpu=',d['cpu'],' features=',d['features'],' os=',d['os'])"

# ---------------------------------------------------------------- C. 构建
say "构建 astral-tund (armv7, iOS >= $MIN_IOS)"
cd "$CRATE"

rustup target list --installed | grep -q armv7s-apple-ios || warn "armv7s-apple-ios 未安装（build-std 模式下通常不需要）"

export IPHONEOS_DEPLOYMENT_TARGET="$MIN_IOS"
export MACOSX_DEPLOYMENT_TARGET="10.15"
# rustc 通过 cc 调 ld：显式指定新版 clang + 老 SDK + armv7
export CC_armv7_ios6="$(xcrun --sdk macosx -f clang)"
export CFLAGS_armv7_ios6="-arch armv7 -miphoneos-version-min=$MIN_IOS -isysroot $SDKROOT -Qunused-arguments"
export RUSTFLAGS="${RUSTFLAGS:-} \
  -C link-arg=-arch -C link-arg=armv7 \
  -C link-arg=-miphoneos-version-min=$MIN_IOS \
  -C link-arg=-isysroot -C link-arg=$SDKROOT \
  -C link-arg=-L$SDKROOT/usr/lib"

# nightly 现在要求显式开启 json target spec 支持；build-std 用来给 Tier-3 目标编 std。
# 不接 tail，保留完整日志（workflow 会 tee 到 ci-out/build.log）。
cargo +nightly -Zjson-target-spec -Z build-std=std,panic_abort \
  build --release --target "$SPEC"

BIN="$CRATE/target/armv7-ios6/release/astral-tund"
[ -f "$BIN" ] || die "产物不存在: $BIN"

# ---------------------------------------------------------------- D. 后处理
say "校验产物架构"
file "$BIN" | sed 's/^/    /'
if command -v otool >/dev/null; then
  otool -l "$BIN" | grep -A4 LC_VERSION_MIN_IPHONEOS | sed 's/^/    /' || warn "没读到 LC_VERSION_MIN_IPHONEOS"
  otool -L "$BIN" | head -12 | sed 's/^/    /'
fi

say "签名（越狱设备用 ad-hoc）"
if command -v ldid >/dev/null; then
  ldid -S "$BIN" && echo "    ldid -S 完成"
else
  warn "没有 ldid：brew install ldid（或 CI 里装），跳过签名"
fi

mkdir -p "$OUT"
cp "$BIN" "$OUT/astral-tund"
(cd "$OUT" && shasum -a 256 astral-tund > astral-tund.sha256)
say "产物: $OUT/astral-tund"
cat "$OUT/astral-tund.sha256"
