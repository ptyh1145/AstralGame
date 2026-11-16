"""Build App + CLI + LaunchDaemon into a jailbreak .deb.

deb is an ar archive with three members:
  debian-binary   "2.0\n"
  control.tar.gz  control / postinst / prerm
  data.tar.gz     actual file tree

macOS has no dpkg-deb, so this writes ar by hand (simple format).

Usage: python3 make_deb.py <dist_dir> <output.deb>
  dist_dir should contain: AstralGame.app/  astral-tund
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
Description: Astral Game iOS LAN multiplayer client (jailbreak)
 Native UIKit UI + EasyTier networking core.
 Daemon runs as root (needs utun). App controls it via /var/run/astral.sock.
"""

POSTINST = """#!/bin/bash
# postinst: runs AFTER unpack (all files are on disk).
# So daemon setup (configure) is correct here; keep every step non-fatal.
set -u
chmod 755 /usr/bin/astral-tund 2>/dev/null || true
chmod 755 /Applications/AstralGame.app/AstralGame 2>/dev/null || true

# Stop any old daemon first (unload never depends on new files).
launchctl unload /Library/LaunchDaemons/com.astral.tund.plist 2>/dev/null || true

# Load daemon ONLY if the plist is really on disk (avoid racing unpack).
# Never fail the package if launchd is old/missing.
if [ -f /Library/LaunchDaemons/com.astral.tund.plist ]; then
  launchctl load /Library/LaunchDaemons/com.astral.tund.plist 2>/dev/null || true
fi

# Refresh springboard icon (differs per jailbreak; try them all).
uicache 2>/dev/null || killall -9 SpringBoard 2>/dev/null || true

echo "Astral Game installed. Daemon: /usr/bin/astral-tund --daemon"
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
    <!-- KeepAlive on abnormal exit only, with 10s throttle (avoid restart loop) -->
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
    """entries: [(arcname, bytes, mode)] -> gzip-compressed tar bytes"""
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
    # GNU ar format: all numeric fields must be RIGHT-aligned (number on the right, spaces on the left).
    # Before, ljust (left-aligned) made old dpkg compute wrong offsets/sizes,
    # passing truncated data to the tar subprocess -> Broken pipe.
    hdr = (
        name.ljust(16)
        + str(0).rjust(12)      # mtime
        + str(0).rjust(6)       # uid
        + str(0).rjust(6)       # gid
        + "100644".rjust(8)     # mode (octal)
        + str(len(data)).rjust(10)  # size (decimal)
        + "`\n"
    ).encode("ascii")
    out = hdr + data
    if len(data) % 2:
        out += b"\n"
    return out


import shutil
import subprocess
import tempfile


def build_pkg_tree(dist):
    """Write the pkg directory tree for dpkg-deb --build.
    Returns (tree_dir, data_entries_count). Caller must clean up tree_dir.
    """
    app = os.path.join(dist, "AstralGame.app")
    cli = os.path.join(dist, "astral-tund")
    tree = tempfile.mkdtemp(prefix="astral_deb_")

    # DEBIAN control scripts (must be 755 for postinst/prerm)
    debian = os.path.join(tree, "DEBIAN")
    os.makedirs(debian)
    def write(fn, data, mode):
        p = os.path.join(debian, fn)
        with open(p, "wb") as f:
            f.write(data)
        os.chmod(p, mode)
    write("control", CONTROL.encode("utf-8"), 0o644)
    write("postinst", POSTINST.encode("utf-8"), 0o755)
    write("prerm", PRERM.encode("utf-8"), 0o755)

    # Applications/AstralGame.app  (SpringBoard only scans /Applications for icons)
    dst_app = os.path.join(tree, "Applications", "AstralGame.app")
    shutil.copytree(app, dst_app, symlinks=True)
    bin_path = os.path.join(dst_app, "AstralGame")
    if os.path.exists(bin_path):
        os.chmod(bin_path, 0o755)

    # usr/bin/astral-tund
    dst_usr = os.path.join(tree, "usr", "bin")
    os.makedirs(dst_usr, exist_ok=True)
    shutil.copy2(cli, os.path.join(dst_usr, "astral-tund"))
    os.chmod(os.path.join(dst_usr, "astral-tund"), 0o755)

    # Library/LaunchDaemons/com.astral.tund.plist
    dst_ld = os.path.join(tree, "Library", "LaunchDaemons")
    os.makedirs(dst_ld, exist_ok=True)
    with open(os.path.join(dst_ld, "com.astral.tund.plist"), "wb") as f:
        f.write(LAUNCHD.encode("utf-8"))
    os.chmod(os.path.join(dst_ld, "com.astral.tund.plist"), 0o644)

    n = 0
    for _, _, files in os.walk(tree):
        n += len(files)
    return tree, n


def build_with_dpkg_deb(dist, out):
    """Use real dpkg-deb (standard ar/gzip/tar format every dpkg accepts)."""
    tree, n = build_pkg_tree(dist)
    try:
        cmd = ["dpkg-deb", "--build", "--root-owner-group", "-Zgzip", tree, out]
        subprocess.run(cmd, check=True)
    finally:
        shutil.rmtree(tree, ignore_errors=True)
    return n


def main():
    dist, out = sys.argv[1], sys.argv[2]
    app = os.path.join(dist, "AstralGame.app")
    cli = os.path.join(dist, "astral-tund")
    for p in (app, cli):
        if not os.path.exists(p):
            raise SystemExit("missing " + p)

    dpkg_deb = shutil.which("dpkg-deb")
    if dpkg_deb:
        n = build_with_dpkg_deb(dist, out)
        sz = os.path.getsize(out)
        print("deb: %s (%.1f MB) [dpkg-deb]" % (out, sz / 1048576))
        print("  data entries:", n)
        return

    print("!! dpkg-deb not found; falling back to hand-built ar (may not install on old dpkg)")
    # ---- fallback: hand-built ar (standard gzip deflate) ----
    data_entries = []
    for root, dirs, files in os.walk(app):
        rel_root = os.path.relpath(root, dist).replace(os.sep, "/")
        for d in dirs:
            data_entries.append(("./Applications/" + rel_root + "/" + d + "/", b"", 0o755))
        for fn in files:
            full = os.path.join(root, fn)
            with open(full, "rb") as f:
                body = f.read()
            mode = 0o755 if fn == "AstralGame" else 0o644
            data_entries.append(("./Applications/" + rel_root + "/" + fn, body, mode))

    with open(cli, "rb") as f:
        data_entries.append(("./usr/bin/astral-tund", f.read(), 0o755))
    data_entries.append(
        ("./Library/LaunchDaemons/com.astral.tund.plist",
         LAUNCHD.encode("utf-8"), 0o644))

    data_gz = gz(tar_gz(data_entries))

    ctrl_entries = [
        ("./control", CONTROL.encode("utf-8"), 0o644),
        ("./postinst", POSTINST.encode("utf-8"), 0o755),
        ("./prerm", PRERM.encode("utf-8"), 0o755),
    ]
    ctrl_gz = gz(tar_gz(ctrl_entries))

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