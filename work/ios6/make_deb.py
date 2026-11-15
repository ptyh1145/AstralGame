"""把 App + CLI + LaunchDaemon 打成越狱用的 .deb。

deb 就是 ar 归档，里面三个成员：
  debian-binary   "2.0\n"
  control.tar.gz  control / postinst / prerm
  data.tar.gz     实际文件树

macOS 没有 dpkg-deb，所以这里手写（ar 格式很简单）。

用法: python3 make_deb.py <dist_dir> <output.deb>
  dist_dir 里应有: AstralGame.app/  astral-tund
"""
import io
import os
import sys
import tarfile

CONTROL = """Package: com.astral.game.ios
Name: Astral Game
Version: 1.0
Architecture: iphoneos-arm
Section: Games
Priority: optional
Depends: firmware (>= 8.0)
Maintainer: astral
Description: Astral Game iOS 联机客户端（越狱版）
 原生 UIKit 界面 + EasyTier 组网核心。
 守护进程以 root 运行（建 utun 需要），App 通过 /var/run/astral.sock 控制。
"""

POSTINST = """#!/bin/bash
set -e
chmod 755 /usr/bin/astral-tund 2>/dev/null || true
chmod 755 /Applications/AstralGame.app/AstralGame 2>/dev/null || true

# 守护进程：以 root 常驻，负责建 utun + 跑 EasyTier
launchctl unload /Library/LaunchDaemons/com.astral.tund.plist 2>/dev/null || true
launchctl load /Library/LaunchDaemons/com.astral.tund.plist 2>/dev/null || true

# 刷新桌面图标（不同越狱环境命令不一样，都试一遍）
uicache 2>/dev/null || killall -9 SpringBoard 2>/dev/null || true

echo "Astral Game 安装完成。守护进程: /usr/bin/astral-tund --daemon"
exit 0
"""

PRERM = """#!/bin/bash
launchctl unload /Library/LaunchDaemons/com.astral.tund.plist 2>/dev/null || true
rm -f /var/run/astral.sock
exit 0
"""

LAUNCHD = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.astral.tund</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/astral-tund</string>
        <string>--daemon</string>
    </array>
    <key>RunAtLoad</key><true/>
    <!-- 不要 KeepAlive=true：万一守护进程因 bind 失败等原因退出，
         launchd 会立刻重拉，形成无限重启循环（发热/耗电/日志暴涨）。
         这里只在「异常退出」时重启，并强制 10 秒节流。 -->
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key><false/>
    </dict>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>UserName</key><string>root</string>
    <key>StandardOutPath</key><string>/var/log/astral-tund.log</string>
    <key>StandardErrorPath</key><string>/var/log/astral-tund.log</string>
</dict>
</plist>
"""


def tar_gz(entries, gz_mode="w:gz"):
    """entries: [(arcname, bytes, mode)] -> gzip 压缩的 tar bytes"""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as tf:
        for arcname, data, mode in entries:
            ti = tarfile.TarInfo(arcname)
            ti.size = len(data)
            ti.mode = mode
            ti.mtime = 0
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = "root"
            tf.addfile(ti, io.BytesIO(data))
    return raw.getvalue()


def gz(data):
    import gzip
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", mtime=0) as f:
        f.write(data)
    return buf.getvalue()


def ar_member(name, data):
    assert len(name) <= 16, name
    hdr = (
        name.ljust(16)
        + "0".ljust(12)      # mtime
        + "0".ljust(6)       # uid
        + "0".ljust(6)       # gid
        + "100644".ljust(8)  # mode
        + str(len(data)).ljust(10)
        + "`\n"
    ).encode("ascii")
    out = hdr + data
    if len(data) % 2:
        out += b"\n"
    return out


def main():
    dist, out = sys.argv[1], sys.argv[2]
    app = os.path.join(dist, "AstralGame.app")
    cli = os.path.join(dist, "astral-tund")
    for p in (app, cli):
        if not os.path.exists(p):
            raise SystemExit("缺少 " + p)

    # ---- data.tar.gz：App + CLI + LaunchDaemon ----
    data_entries = []
    for root, dirs, files in os.walk(app):
        rel_root = os.path.relpath(root, dist).replace(os.sep, "/")
        for d in dirs:
            data_entries.append(("./" + rel_root + "/" + d + "/", b"", 0o755))
        for fn in files:
            full = os.path.join(root, fn)
            with open(full, "rb") as f:
                body = f.read()
            mode = 0o755 if fn == "AstralGame" else 0o644
            data_entries.append(("./" + rel_root + "/" + fn, body, mode))

    with open(cli, "rb") as f:
        data_entries.append(("./usr/bin/astral-tund", f.read(), 0o755))
    data_entries.append(
        ("./Library/LaunchDaemons/com.astral.tund.plist",
         LAUNCHD.encode("utf-8"), 0o644))

    data_gz = gz(tar_gz(data_entries))

    # ---- control.tar.gz ----
    ctrl_entries = [
        ("./control", CONTROL.encode("utf-8"), 0o644),
        ("./postinst", POSTINST.encode("utf-8"), 0o755),
        ("./prerm", PRERM.encode("utf-8"), 0o755),
    ]
    ctrl_gz = gz(tar_gz(ctrl_entries))

    # ---- ar 归档 ----
    blob = b"!<arch>\n"
    blob += ar_member("debian-binary", b"2.0\n")
    blob += ar_member("control.tar.gz", ctrl_gz)
    blob += ar_member("data.tar.gz", data_gz)
    with open(out, "wb") as f:
        f.write(blob)

    print("deb: %s (%.1f MB)" % (out, len(blob) / 1048576))
    print("  data entries:", len(data_entries))


if __name__ == "__main__":
    main()