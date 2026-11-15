#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把 Mach-O 的 iOS 最低版本改回 6.0。

背景：rustc 1.102 对 armv7s-apple-ios 的 minimum deployment target 是 10.0，
它会强制传 `-target armv7s-apple-ios10.0.0`，覆盖我们给 clang 的
`-miphoneos-version-min=6.0`，于是产物的 LC_VERSION_MIN_IPHONEOS 会被标成 10.0。
本脚本把版本命令里的 minos 改回 6.0、sdk 改回 6.1，让包能装到 iOS 6。

同时支持两种 load command：
  * LC_VERSION_MIN_IPHONEOS (0x25)  —— 老式（armv7 32-bit 常见）
  * LC_BUILD_VERSION       (0x32)  —— 新式（含 platform 字段）

必须在 `ldid -S` 签名**之前**运行（改文件内容会让签名失效）。

用法: python3 fix_macho_ver.py <binary> [min_x.y] [sdk_x.y]
"""
import struct
import sys

LC_VERSION_MIN_IPHONEOS = 0x25
LC_BUILD_VERSION = 0x32
PLATFORM_IOS = 2
MH_MAGIC = 0xFEEDFACE
MH_MAGIC_64 = 0xFEEDFACF


def ver(text):
    """"6.1" -> (6<<16)|(1<<8)|0"""
    parts = (text.split(".") + ["0", "0"])[:3]
    return (int(parts[0]) << 16) | (int(parts[1]) << 8) | int(parts[2])


def main():
    path = sys.argv[1]
    min_v = ver(sys.argv[2] if len(sys.argv) > 2 else "6.0")
    sdk_v = ver(sys.argv[3] if len(sys.argv) > 3 else "6.1")

    data = bytearray(open(path, "rb").read())
    if len(data) < 32:
        raise SystemExit("文件太小，不是 Mach-O")

    magic = struct.unpack_from("<I", data, 0)[0]
    if magic == MH_MAGIC:
        header_size = 28
    elif magic == MH_MAGIC_64:
        header_size = 32
    else:
        raise SystemExit("不是 Mach-O（magic=0x%x）" % magic)

    ncmds = struct.unpack_from("<I", data, 16)[0]
    off = header_size
    patched = []

    for _ in range(ncmds):
        if off + 8 > len(data):
            raise SystemExit("load command 越界")
        cmd, cmdsize = struct.unpack_from("<II", data, off)
        if cmdsize < 8:
            raise SystemExit("非法 cmdsize=%d" % cmdsize)

        if cmd == LC_VERSION_MIN_IPHONEOS:
            # u32 cmd, u32 cmdsize, u32 version, u32 sdk
            struct.pack_into("<II", data, off + 8, min_v, sdk_v)
            patched.append("LC_VERSION_MIN_IPHONEOS")
        elif cmd == LC_BUILD_VERSION:
            # u32 cmd, u32 cmdsize, u32 platform, u32 minos, u32 sdk, u32 ntools
            platform = struct.unpack_from("<I", data, off + 8)[0]
            if platform == PLATFORM_IOS:
                struct.pack_into("<II", data, off + 12, min_v, sdk_v)
                patched.append("LC_BUILD_VERSION")

        off += cmdsize

    if not patched:
        raise SystemExit("没找到 iOS 版本命令，未修改")

    open(path, "wb").write(data)
    print("    修复版本命令: %s -> min=%s sdk=%s"
          % (", ".join(patched),
             sys.argv[2] if len(sys.argv) > 2 else "6.0",
             sys.argv[3] if len(sys.argv) > 3 else "6.1"))


if __name__ == "__main__":
    main()