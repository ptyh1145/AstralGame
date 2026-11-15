#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""给 ring 0.17 打补丁：支持 iOS 32 位 ARM 的汇编。

## 问题

ring 0.17.14 的 build.rs 里，汇编目标是这样选的：

    let asm_target = ASM_TARGETS.iter().find(|t| {
        t.arch == target.arch && t.oss.contains(&target.os.as_ref())
    });

而 `ASM_TARGETS` 中 32 位 `arm` 只有两条：`LINUX_ABI` 和 `horizon`，
**没有 `APPLE_ABI`**。于是 iOS/armv7 编译时 `asm_target` 是 `None`，
`build_c_code()` 直接跳过所有汇编，只有 C 源被编译。

结果就是链接期这些符号全部 undefined：

    _ring_core_0_17_14__sha256_block_data_order_neon
    _ring_core_0_17_14__vpaes_set_encrypt_key
    _ring_core_0_17_14__openssl_poly1305_neon2_addmod
    ... 共 17 个

## 修法

1. 往 `ASM_TARGETS` 插入 `arm + APPLE_ABI + "ios32"`。
   `crypto/perlasm/arm-xlate.pl` 本来就支持 `ios32` 这个 flavour
   （会给符号加下划线前缀，正是 Mach-O 需要的）。
2. ring 在"打包模式"（无 `.git`）下会用仓库里预生成的汇编；
   但那批文件是按各平台预生成的，**不包含 ios32**。
   所以强制走"用 perl 现场生成"的分支（`if false` 取代 `if !is_git`）。
   该分支只处理当前 target 的 asm_target，不会去碰 nasm，安全。
3. 顺手把 `force_warnings_into_errors` 关掉，避免第三方汇编的
   warning 被当成错误。

用法: python3 patch_ring_asm.py <registry_src_dir> [更多目录...]
"""
import glob
import os
import sys

INSERT = """    // [astral-ios6-patch] iOS 32-bit ARM 的汇编目标。
    // 上游 ring 0.17 的 ASM_TARGETS 里 arm 只有 Linux/Horizon，
    // 导致 iOS 上 asm_target=None、汇编完全不编译，
    // _ring_core_*_neon 等符号在链接期全部 undefined。
    AsmTarget {
        oss: APPLE_ABI,
        arch: ARM,
        perlasm_format: "ios32",
    },
"""


def patch_file(path):
    src = open(path, encoding="utf-8").read()
    if 'perlasm_format: "ios32"' in src:
        print("    already patched: %s" % path)
        return False

    anchor = "const ASM_TARGETS: &[AsmTarget] = &[\n"
    if anchor not in src:
        print("    !! 找不到 ASM_TARGETS 定义，跳过: %s" % path)
        return False
    src = src.replace(anchor, anchor + INSERT, 1)

    # 强制走"现场生成"分支（只生成当前 target，不碰 nasm）
    old_gen = "    let generated_dir = if !is_git {"
    if old_gen not in src:
        print("    !! 找不到 generated_dir 判断，跳过: %s" % path)
        return False
    src = src.replace(old_gen, "    let generated_dir = if false {", 1)

    # 不要把第三方汇编的 warning 升级成错误
    src = src.replace(
        "let force_warnings_into_errors = is_git;",
        "let force_warnings_into_errors = false;", 1)

    # perlasm 现场生成的 .S 里**没有** `#include <ring-core/asm_base.h>`，
    # 而符号前缀宏（ring_core_x_y_z__）就藏在
    # asm_base.h -> ring_core_generated/prefix_symbols_asm.h 里。
    # 预生成的 .S 自带这行 include，现场生成的不带 —— 不补的话汇编符号
    # 没有前缀，链接时就报 _ring_core_*_sha256_block_data_order_nohw 未定义。
    old_perl = """        args.push(join_components_with_forward_slashes(dst));
        run_command_with_args(perl_exe, &args);"""
    new_perl = """        args.push(join_components_with_forward_slashes(dst));
        run_command_with_args(perl_exe, &args);
        // [astral-ios6-patch] 补上 asm_base.h，让符号前缀宏生效
        if dst.extension().and_then(|e| e.to_str()) == Some("S") {
            if let Ok(body) = fs::read_to_string(dst) {
                if !body.contains("asm_base.h") {
                    let mut with_inc = String::from("#include <ring-core/asm_base.h>\\n");
                    with_inc.push_str(&body);
                    let _ = fs::write(dst, with_inc);
                }
            }
        }"""
    if old_perl not in src:
        print("    !! 找不到 perlasm 调用处，跳过: %s" % path)
        return False
    src = src.replace(old_perl, new_perl, 1)

    open(path, "w", encoding="utf-8", newline="").write(src)
    print("    patched: %s" % path)
    return True


def main():
    roots = sys.argv[1:]
    if not roots:
        raise SystemExit("用法: patch_ring_asm.py <registry_src_dir> [...]")

    candidates = []
    for root in roots:
        candidates += glob.glob(os.path.join(root, "*", "ring-0.17.*", "build.rs"))
        candidates += glob.glob(os.path.join(root, "ring-0.17.*", "build.rs"))

    if not candidates:
        print("    !! 没找到 ring 的 build.rs")
        return 1

    n = 0
    for path in candidates:
        if patch_file(path):
            n += 1
    print("    ring 汇编补丁: 修改 %d 个 build.rs" % n)
    return 0


if __name__ == "__main__":
    sys.exit(main())