#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把依赖里 crate-type 含 cdylib/staticlib 的 Cargo.toml 收敛成 ["rlib"]。

为什么需要：cargo 会为这些 crate 传 --crate-type cdylib/staticlib，
而 rustc 在自定义 target 上处理 darwin cdylib 的 install_name 时会 ICE
（内部编译器错误）。我们只需要把 rlib 链进最终二进制。

cargo 不校验 registry / git checkout 的源码内容，所以直接改文件即可。
支持跨多行的数组写法。
"""
import os, re, sys

ARRAY = re.compile(r'crate-type\s*=\s*\[(.*?)\]', re.S)
BAD = ("cdylib", "staticlib")


def patch_file(path):
    try:
        src = open(path, encoding="utf-8", errors="replace").read()
    except OSError:
        return False
    m = ARRAY.search(src)
    if not m:
        return False
    body = m.group(1)
    if not any(b in body for b in BAD):
        return False
    new = ARRAY.sub('crate-type = ["rlib"]', src, count=1)
    if new == src:
        return False
    try:
        open(path, "w", encoding="utf-8").write(new)
    except OSError:
        return False
    return True


def main():
    roots = sys.argv[1:]
    total = 0
    for root in roots:
        if not os.path.isdir(root):
            continue
        seen = 0
        for dirpath, _dirnames, filenames in os.walk(root):
            if "Cargo.toml" not in filenames:
                continue
            seen += 1
            p = os.path.join(dirpath, "Cargo.toml")
            if patch_file(p):
                try:
                    shown = os.path.relpath(p, os.path.expanduser("~"))
                except ValueError:
                    shown = p
                print("    patched: %s" % shown)
                total += 1
        print("    扫描 %s : %d 个 Cargo.toml" % (root, seen))
    print("    共改写 %d 个 crate" % total)
    return 0


if __name__ == "__main__":
    sys.exit(main())
