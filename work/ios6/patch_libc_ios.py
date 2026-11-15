# 修补 vendored libc：把 utun/ctl 相关符号从 macos-only 放开到 any(macos, ios)。
# 用法: python3 patch_libc_ios.py <libc_src_dir>
import sys, os

root = sys.argv[1]
f = os.path.join(root, "src", "unix", "bsd", "apple", "mod.rs")
s = open(f, encoding="utf-8").read()
orig = s

MACOS_CFG = '#[cfg(target_os = "macos")]'
ANY_CFG = '#[cfg(any(target_os = "macos", target_os = "ios"))]'
MACOS_IF = 'if #[cfg(target_os = "macos")]'
ANY_IF = 'if #[cfg(any(target_os = "macos", target_os = "ios"))]'

def patch_attr(anchor):
    """把 anchor 之前最近的 struct-level #[cfg(target_os=macos)] 改为 any(...)."""
    global s
    i = s.find(anchor)
    if i < 0:
        raise SystemExit("anchor not found: " + anchor)
    j = s.rfind(MACOS_CFG, 0, i)
    if j < 0:
        raise SystemExit("no attr cfg before " + anchor)
    s = s[:j] + ANY_CFG + s[j + len(MACOS_CFG):]

def patch_cfgif(anchor):
    """把 anchor 之前最近的 'if #[cfg(target_os=macos)]' 改为 any(...)."""
    global s
    i = s.find(anchor)
    if i < 0:
        raise SystemExit("anchor not found: " + anchor)
    j = s.rfind(MACOS_IF, 0, i)
    if j < 0:
        raise SystemExit("no cfg_if before " + anchor)
    s = s[:j] + ANY_IF + s[j + len(MACOS_IF):]

patch_attr("pub struct sockaddr_ctl")
patch_attr("pub struct ctl_info")
patch_cfgif("pub const AF_SYS_CONTROL")
patch_cfgif("pub const UTUN_OPT_IFNAME")
patch_cfgif("pub const CTLIOCGINFO")
# ctl_info 结构体里引用了 MAX_KCTL_NAME（kern_control.h），它也在同一个
# macos-only cfg_if 块里，不放开 iOS 会报 E0425。
patch_cfgif("pub const MAX_KCTL_NAME")

open(f, "w", encoding="utf-8", newline="").write(s)
print("patched:", os.path.basename(f))
print("changed:", s != orig)
# 校验所有目标现在都是 any(macos, ios)
checks = {
    "sockaddr_ctl": "pub struct sockaddr_ctl",
    "ctl_info": "pub struct ctl_info",
    "AF_SYS_CONTROL": "pub const AF_SYS_CONTROL",
    "UTUN_OPT_IFNAME": "pub const UTUN_OPT_IFNAME",
    "CTLIOCGINFO": "pub const CTLIOCGINFO",
}
for name, anchor in checks.items():
    if anchor in s:
        # 该符号之前最近的 cfg 必须含 ios
        i = s.find(anchor)
        window = s[max(0, i - 200):i]
        ok = 'target_os = "ios"' in window
        print(("  OK  " if ok else "  FAIL") + " " + name)