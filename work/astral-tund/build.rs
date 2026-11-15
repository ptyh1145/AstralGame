//! 为 iOS/armv7 编译 ring 需要的 ARM 汇编。
//!
//! ## 为什么需要它
//!
//! ring 0.17 的加密实现（sha256/sha512/aes/chacha/bn）**全部是汇编**，
//! 由 build script 调 perlasm 生成或从 `pregenerated/` 取。但在我们的
//! 自定义 target 上，这套流程始终不可靠：
//!
//!   * ring 的 `ASM_TARGETS` 里 32 位 arm 漏了 Apple 条目；
//!   * 补上之后，`.S` 的顶层守卫
//!     `#if !defined(OPENSSL_NO_ASM) && defined(OPENSSL_ARM) && defined(__APPLE__)`
//!     又因为 clang 不再定义 `__ARMEL__` 等原因不成立，
//!     整个文件被预处理成空，于是
//!       ranlib: file: libring_core_*.a(xxx.o) has no symbols
//!     最终表现为链接期 17 个 `_ring_core_*_neon` / `_nohw` 符号 undefined。
//!
//! 与其继续猜 ring build script 的行为（它的输出目录还受 host/target
//! profile、`.fingerprint` 缓存影响），不如**我们自己编这几个 .S**：
//! 编译命令完全可控，宏、include、架构都显式指定。
//!
//! 汇编文件是本地用 perl 预生成的 `pregenerated/*-ios32.S`，
//! 符号前缀宏来自 `pregenerated/ring_core_generated/prefix_symbols_asm.h`。

use std::env;
use std::path::PathBuf;

/// ring 0.17 里所有 32 位 ARM 的 perlasm 源文件（已预生成为 *-ios32.S）
const ASM_STEMS: &[&str] = &[
    "bsaes-armv7",
    "ghash-armv4",
    "vpaes-armv7",
    "armv4-mont",
    "chacha-armv4",
    "sha256-armv4",
    "sha512-armv4",
];

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    println!("cargo:rerun-if-changed=build.rs");

    // 只在 iOS/armv7 下接管；其他平台交给 ring 自己处理
    if target_os != "ios" || target_arch != "arm" {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ring = manifest.join("../vendor/ring-0.17.14");
    let pregen = ring.join("pregenerated");

    let mut asm_files = Vec::new();
    for stem in ASM_STEMS {
        let f = pregen.join(format!("{}-ios32.S", stem));
        if f.exists() {
            println!("cargo:rerun-if-changed={}", f.display());
            asm_files.push(f);
        } else {
            // 缺文件就让编译失败，避免又变成"符号静默缺失"
            panic!("缺少预生成的汇编: {}", f.display());
        }
    }

    let mut build = cc::Build::new();
    // 符号前缀宏（ring_core_0_17_14__xxx）来自 <ring_core_generated/...>
    build.include(&pregen);
    // target.h / asm_base.h
    build.include(ring.join("include"));

    // 钉死守卫宏，确保 .S 内容不会被预处理掉
    build.define("OPENSSL_ARM", "1");
    build.define("OPENSSL_32_BIT", "1");
    build.define("__APPLE__", "1");

    // 架构与 SDK 由 CFLAGS_<target> 提供（build-ci.sh 里已设），
    // 这里再显式补一份，防止 cc-rs 取不到。
    build.flag("-arch").flag("armv7s");

    for f in &asm_files {
        build.file(f);
    }

    build.compile("astral_ring_asm");
}