"""修 vendored ring 的 32 位 iOS 汇编问题（一次性打好，CI 不再跑 patch）。

## 背景

ring 0.17 对 32 位 arm 的汇编配置有两处缺口，导致 iOS/armv7 链接时
17 个 `_ring_core_*` 符号 undefined：

1. `ASM_TARGETS` 里 arm 只配了 Linux/Horizon，**没有 Apple** →
   `asm_target=None` → 所有汇编不编译。
   修法：加 `arm + APPLE_ABI + "ios32"`，并把 `*_ios32.S` 预生成好
   放进 `pregenerated/`（已由本地 perl 完成）。

2. `crypto/poly1305/poly1305_arm_asm.S` 是 SUPERCOP 的 neon2 汇编，
   但**被 `#if ... && defined(__ELF__)` 包裹**，在 Mach-O 上整个变空，
   于是 `_openssl_poly1305_neon2_*` 缺失。
   这段汇编是 ELF 语法（qhasm 生成），改成 Mach-O 成本很高，
   所以在 iOS 上改用通用 C 实现 `poly1305.c`，并把 `*_neon` 符号名
   映射过去（Rust 侧 `ffi_arm_neon.rs` 声明的是 neon 名）。

用法: python3 patch_ring_vendored.py <ring_dir>
"""
import os
import sys

ring = sys.argv[1]
build_rs = os.path.join(ring, "build.rs")
poly_c = os.path.join(ring, "crypto", "poly1305", "poly1305.c")

# ---------------------------------------------------------------- 1. build.rs
src = open(build_rs, encoding="utf-8").read()
changed = False

if 'perlasm_format: "ios32"' not in src:
    anchor = "const ASM_TARGETS: &[AsmTarget] = &[\n"
    if anchor not in src:
        raise SystemExit("找不到 ASM_TARGETS")
    src = src.replace(anchor, anchor + """    // [astral-ios6] iOS 32 位 ARM 的汇编目标（上游漏了，见文件头注释）
    AsmTarget {
        oss: APPLE_ABI,
        arch: ARM,
        perlasm_format: "ios32",
    },
""", 1)
    changed = True

# arm 的源文件里加上通用 poly1305.c
if '"crypto/poly1305/poly1305.c"' not in src:
    old_poly = '    (&[ARM], "crypto/poly1305/poly1305_arm.c"),'
    if old_poly not in src:
        raise SystemExit("找不到 poly1305_arm.c 条目")
    src = src.replace(old_poly, old_poly +
                      '\n    // [astral-ios6] 通用 C 实现（iOS 上没有 neon2 汇编时用）'
                      '\n    (&[ARM], "crypto/poly1305/poly1305.c"),', 1)
    changed = True

# iOS 下排除 poly1305_arm.c（它依赖 ELF 才能汇编的 neon2）
if "poly1305_arm.c 依赖 ELF 汇编" not in src:
    old_filter = """    let core_srcs = sources_for_arch(&target.arch)
        .into_iter()
        .filter(|p| !is_perlasm(p))"""
    if old_filter not in src:
        raise SystemExit("找不到 core_srcs 过滤处")
    src = src.replace(old_filter, """    let core_srcs = sources_for_arch(&target.arch)
        .into_iter()
        .filter(|p| !is_perlasm(p))
        // [astral-ios6] poly1305_arm.c 依赖 ELF 汇编，Mach-O 上不可用，
        // 改用 poly1305.c 的通用实现（见该文件末尾的 neon 别名）。
        .filter(|p| {
            !(APPLE_ABI.contains(&target.os.as_str())
                && p.file_name().and_then(|n| n.to_str()) == Some("poly1305_arm.c"))
        })""", 1)
    changed = True

if changed:
    open(build_rs, "w", encoding="utf-8", newline="").write(src)
    print("patched:", build_rs)
else:
    print("build.rs 已是最新")

# ---------------------------------------------------------------- 2. poly1305.c
src = open(poly_c, encoding="utf-8").read()
if "CRYPTO_poly1305_init_neon" not in src:
    src = src.rstrip() + """

#if defined(OPENSSL_ARM) && defined(__APPLE__)
// [astral-ios6] iOS/Mach-O 上用不了 neon2 汇编（poly1305_arm_asm.S 是
// ELF 语法），所以不编译 poly1305_arm.c。Rust 侧 ffi_arm_neon.rs 声明的
// 是 *_neon 符号，这里把它们映射到本文件的通用实现。
void CRYPTO_poly1305_init_neon(struct poly1305_state_st *state,
                               const uint8_t key[32]) {
  CRYPTO_poly1305_init(state, key);
}

void CRYPTO_poly1305_update_neon(struct poly1305_state_st *state,
                                 const uint8_t *in, size_t len) {
  CRYPTO_poly1305_update(state, in, len);
}

void CRYPTO_poly1305_finish_neon(struct poly1305_state_st *state,
                                 uint8_t mac[16]) {
  CRYPTO_poly1305_finish(state, mac);
}
#endif
"""
    open(poly_c, "w", encoding="utf-8", newline="").write(src)
    print("patched:", poly_c)
else:
    print("poly1305.c 已是最新")

# ---------------------------------------------------------------- 3. 校验
b = open(build_rs, encoding="utf-8").read()
assert 'perlasm_format: "ios32"' in b
assert '"crypto/poly1305/poly1305.c"' in b
assert "poly1305_arm.c 依赖 ELF 汇编" in b
p = open(poly_c, encoding="utf-8").read()
assert "CRYPTO_poly1305_init_neon" in p
print("校验 OK")