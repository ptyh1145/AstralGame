# 修补 vendored libc：把 src/unix/bsd/apple/mod.rs 里所有 macos-only 的 cfg
# 一次性放开到 any(macos, ios)。
#
# 背景：nix/tun 在 iOS 分支里引用 libc 的 utun/ctl 符号（sockaddr_ctl、
# AF_SYS_CONTROL、SYSPROTO_CONTROL、UTUN_OPT_IFNAME、ctl_info、CTLIOCGINFO），
# 这些在 libc 里全被 #[cfg(target_os = "macos")] 挡着。而它们互相引用形成
# 依赖网（如 ctl_info 用 MAX_KCTL_NAME、ifreq 联合用 in6_ifstat 等），
# 逐个放开会连锁报 E0425。所以干脆把整个 apple 模块的 macos 门全放开，
# 让 iOS 编译期按 macOS 语义暴露同一组符号（Darwin 共用 libSystem/内核 ABI）。
#
# 用法: python3 patch_libc_ios.py <libc_src_dir>
import sys, os, glob

root = sys.argv[1]
targets = []
for pat in [
    "src/unix/bsd/apple/mod.rs",
    "src/unix/bsd/apple/b32/mod.rs",
    "src/unix/bsd/apple/b64/mod.rs",
    "src/unix/bsd/apple/b64/aarch64/mod.rs",
    "src/unix/bsd/apple/b64/x86_64/mod.rs",
]:
    p = os.path.join(root, pat)
    if os.path.exists(p):
        targets.append(p)

old = 'cfg(target_os = "macos")'
new = 'cfg(any(target_os = "macos", target_os = "ios"))'

total = 0
for f in targets:
    s = open(f, encoding="utf-8").read()
    n = s.count(old)
    if n:
        s = s.replace(old, new)
        open(f, "w", encoding="utf-8", newline="").write(s)
        print("patched %s : %d 处" % (os.path.basename(f), n))
        total += n

print("共修改 %d 处 cfg" % total)

# 校验关键符号现在都暴露给 iOS
mod = os.path.join(root, "src/unix/bsd/apple/mod.rs")
s = open(mod, encoding="utf-8").read()
checks = ["sockaddr_ctl", "ctl_info", "AF_SYS_CONTROL", "SYSPROTO_CONTROL",
          "UTUN_OPT_IFNAME", "CTLIOCGINFO", "MAX_KCTL_NAME",
          "in6_ifstat", "icmp6_ifstat", "in6_addrlifetime", "proc_fdinfo"]
for a in checks:
    i = s.find("pub struct " + a)
    if i < 0:
        i = s.find("pub const " + a)
    if i < 0:
        print("FAIL 找不到符号 " + a)
        continue
    w = s[max(0, i - 160):i]
    ok = 'target_os = "ios"' in w
    print(("OK  " if ok else "??  ") + a)