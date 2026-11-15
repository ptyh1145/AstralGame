#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""修补 iOS 6.1 SDK 里的旧式 .tbd 文件，让现代 ld64 能解析。

问题：这批 SDK 从 Xcode 4.6 时代提取，tbd 是 tapi-tbd-v3 格式，且
platform 字段写成 `platform:              (null)`。Xcode 16 的 ld64 不再
接受 (null)，报：

    ld: malformed file
    .../SystemConfiguration.tbd:3:24: error: unknown platform
    platform:              (null)

修法：把 v3 tbd 的 `platform: (null)` 改成 `platform: ios`。
v4 格式（用 targets: 而不是 platform:）不动。

用法: python3 fix_sdk_tbd.py <sdk_path>
"""
import os
import re
import sys

sdk = sys.argv[1]

# 只匹配「行首 platform: 后面跟 (null)」；保留缩进/对齐无关紧要
PATTERN = re.compile(r"^(\s*platform:\s+)\(null\)\s*$", re.MULTILINE)
# v3 头部
V3 = "!tapi-tbd-v3"

fixed = skipped = 0
tbd_total = 0

for base, _dirs, files in os.walk(sdk):
    for name in files:
        if not name.endswith(".tbd"):
            continue
        tbd_total += 1
        path = os.path.join(base, name)
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as f:
                text = f.read()
        except OSError as e:
            print("    跳过 %s (%s)" % (path, e))
            skipped += 1
            continue

        if V3 not in text:
            skipped += 1
            continue

        new, n = PATTERN.subn(r"\1ios", text)
        if n:
            with open(path, "w", encoding="utf-8", newline="") as f:
                f.write(new)
            fixed += 1

print("    tbd 文件总数: %d" % tbd_total)
print("    修正 platform: (null) -> ios : %d 个" % fixed)
print("    跳过(非v3或非null)          : %d 个" % skipped)