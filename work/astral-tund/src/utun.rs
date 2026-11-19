//! iOS / macOS 的 utun 设备（PF_SYSTEM + com.apple.net.utun_control）。
//!
//! iOS 上没有 NetworkExtension（iOS 9+），所以拿不到 VpnService 那种 fd；
//! 但内核仍然提供 macOS 同款的 utun 控制通道，**root 进程**可以自己开一个。
//! 越狱设备上 daemon 以 root 运行，即可用本模块拿到 fd 交给 EasyTier。
//!
//! 报文格式（关键，与 EasyTier 的 iOS 分支一致）：
//!   read()  -> [4 字节 AF 头][IP 包]      （头里是 AF_INET/AF_INET6，大端 u32）
//!   write() -> 必须自己补上这 4 字节头
//! EasyTier 在 `create_dev_for_mobile` 里对 iOS 设了 `packet_information(false)`
//! 并手工补头（proto = PF_INET/PF_INET6），正好就是 utun 的约定，
//! 所以我们直接把裸 utun fd 喂给它即可，**核心代码无需改动**。
//!
//! ## 接口配置
//!
//! 地址 / 掩码 / UP 全部走 **ioctl 直下内核**，不依赖任何外部命令——
//! iOS 6 到现代 iOS 的 syscall 语义一致，也不会因为 Cydia 没装 inetutils 就瘸腿。
//!
//! 只有"网段路由"仍需外部 `route` 命令，而它的路径是**运行时探测**的
//! （`/sbin` `/bin` `/usr/sbin` `/usr/bin` `/usr/local/*` 再 $PATH，见 `SEARCH_DIRS`），
//! 启动时会把探测结果打进 `[probe]` 日志，一眼就能看出设备上有什么、没有什么。

#![allow(non_camel_case_types)]

use anyhow::{bail, Context, Result};
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// ==================== utun 控制通道 ====================

const PF_SYSTEM: libc::c_int = 32;
const AF_SYSCONTROL: u8 = 2;
const SYSPROTO_CONTROL: libc::c_int = 2;
/// _IOWR('N', 3, struct ctl_info)
const CTLIOCGINFO: libc::c_ulong = 0xc064_4e03;
const UTUN_OPT_IFNAME: libc::c_int = 2;
const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control";

#[repr(C)]
struct CtlInfo {
    ctl_id: u32,
    ctl_name: [libc::c_char; 96],
}

#[repr(C)]
struct SockaddrCtl {
    sc_len: u8,
    sc_family: u8,
    ss_sysaddr: u16,
    sc_id: u32,
    sc_unit: u32,
    sc_reserved: [u32; 5],
}

/// 打开 `utun{unit}`。注意：内核的 `sc_unit` 是 **unit + 1**
/// （要 utun0 就传 sc_unit = 1，要 utun3 就传 4）。
pub fn open_utun(unit: u32) -> Result<RawFd> {
    unsafe {
        let fd = libc::socket(PF_SYSTEM, libc::SOCK_DGRAM, SYSPROTO_CONTROL);
        if fd < 0 {
            bail!(
                "socket(PF_SYSTEM, SYSPROTO_CONTROL) 失败: {}（需要 root）",
                std::io::Error::last_os_error()
            );
        }

        let mut info = CtlInfo { ctl_id: 0, ctl_name: [0; 96] };
        for (i, b) in UTUN_CONTROL_NAME.iter().enumerate() {
            info.ctl_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(fd, CTLIOCGINFO, &mut info as *mut CtlInfo) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            bail!("ioctl(CTLIOCGINFO) 失败: {e}（utun 控制不可用 / 无权限）");
        }

        let addr = SockaddrCtl {
            sc_len: std::mem::size_of::<SockaddrCtl>() as u8,
            sc_family: AF_SYSCONTROL,
            ss_sysaddr: AF_SYSCONTROL as u16,
            sc_id: info.ctl_id,
            sc_unit: unit + 1,
            sc_reserved: [0; 5],
        };
        if libc::connect(
            fd,
            &addr as *const SockaddrCtl as *const libc::sockaddr,
            std::mem::size_of::<SockaddrCtl>() as libc::socklen_t,
        ) < 0
        {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            bail!("connect(utun unit={}) 失败: {e}", unit + 1);
        }
        Ok(fd)
    }
}

/// 取这个 fd 对应的接口名（utun0 / utun3 …）。
pub fn ifname(fd: RawFd) -> Result<String> {
    let mut buf = [0u8; 32];
    let mut len = buf.len() as libc::socklen_t;
    unsafe {
        if libc::getsockopt(
            fd,
            SYSPROTO_CONTROL,
            UTUN_OPT_IFNAME,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
        ) < 0
        {
            bail!("getsockopt(UTUN_OPT_IFNAME) 失败: {}", std::io::Error::last_os_error());
        }
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
}

// ==================== 外部命令探测（启动时做一次） ====================

/// 搜索顺序：先系统固定目录，再 $PATH。**不写死任何一个路径。**
const SEARCH_DIRS: &[&str] = &[
    "/sbin",
    "/bin",
    "/usr/sbin",
    "/usr/bin",
    "/usr/local/sbin",
    "/usr/local/bin",
    "/opt/sbin",
    "/opt/bin",
];

#[derive(Debug, Clone)]
pub struct Tools {
    /// 仅作兜底：正常情况下地址配置走 ioctl，用不到它
    pub ifconfig: Option<PathBuf>,
    /// 加网段路由需要它
    pub route: Option<PathBuf>,
}

static TOOLS: OnceLock<Tools> = OnceLock::new();

/// 取（并首次触发）探测结果。main 会在启动最早期调用一次，把清单打进日志。
pub fn tools() -> &'static Tools {
    TOOLS.get_or_init(|| {
        let t = Tools { ifconfig: which("ifconfig"), route: which("route") };

        let show = |k: &str, v: &Option<PathBuf>| match v {
            Some(p) => println!("[probe] {k}: {}", p.display()),
            None => println!("[probe] {k}: 未找到"),
        };
        show("ifconfig", &t.ifconfig);
        show("route", &t.route);

        println!(
            "[probe] 地址配置: ioctl（内置，始终可用）{}",
            if t.ifconfig.is_some() { "；ifconfig 仅作兜底" } else { "" }
        );
        if t.route.is_none() {
            println!(
                "[probe] 警告: 没有任何 route 命令，网段路由需要手动添加\
                 （Cydia 装 inetutils 或 network-cmds 可解决）"
            );
        }
        t
    })
}

fn which(cmd: &str) -> Option<PathBuf> {
    for d in SEARCH_DIRS {
        let p = Path::new(d).join(cmd);
        if is_exec_file(&p) {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            let p = d.join(cmd);
            if is_exec_file(&p) {
                return Some(p);
            }
        }
    }
    None
}

fn is_exec_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ==================== 接口地址配置（ioctl 直下内核，零外部依赖） ====================

const IFNAMSIZ: usize = 16;
/// sizeof(struct ifreq) = 16（名字）+ 16（union 最大成员 struct sockaddr）
const IFREQ_LEN: usize = 32;

// _IOW('i', n, struct ifreq) = 0x80000000 | (32 << 16) | ('i' << 8) | n
const SIOCSIFADDR: libc::c_ulong = 0x8020_690c;
const SIOCSIFDSTADDR: libc::c_ulong = 0x8020_690e;
const SIOCSIFFLAGS: libc::c_ulong = 0x8020_6910;
const SIOCSIFNETMASK: libc::c_ulong = 0x8020_6916;
const SIOCSIFMTU: libc::c_ulong = 0x8020_6934;
// _IOWR('i', n, struct ifreq) = 0xc0000000 | (32 << 16) | ('i' << 8) | n
const SIOCGIFFLAGS: libc::c_ulong = 0xc020_6911;
const SIOCGIFADDR: libc::c_ulong = 0xc020_6921;
const SIOCGIFMTU: libc::c_ulong = 0xc020_6933;
const IFF_UP: libc::c_short = 0x1;

/// 与 EasyTier 虚拟网络的 MTU 保持一致（默认 1380，已含加密 + FEC 开销）。
///
/// 不设的话 utun 沿用默认的大 MTU，内核会把 1500 字节的包原样丢进来，
/// EasyTier 再封装就超过底层链路 MTU —— 大包在途中被丢，表现是
/// 「能看到对面节点、ping 得通小包，但 TCP 握手/传输起不来」。
pub const TUN_MTU: i32 = 1380;

#[repr(C)]
struct IfReq {
    ifr_name: [libc::c_char; IFNAMSIZ],
    /// union ifr_ifru 的原始字节；Darwin 上最大成员是 struct sockaddr（16 字节）
    ifr_ifru: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<IfReq>() == IFREQ_LEN);

impl IfReq {
    fn new(name: &str) -> Result<Self> {
        if name.is_empty() || name.len() >= IFNAMSIZ {
            bail!("接口名非法（长度须 1..={}）: {name:?}", IFNAMSIZ - 1);
        }
        let mut r = IfReq { ifr_name: [0; IFNAMSIZ], ifr_ifru: [0; 16] };
        for (i, b) in name.as_bytes().iter().enumerate() {
            r.ifr_name[i] = *b as libc::c_char;
        }
        Ok(r)
    }

    /// 往 union 里写 sockaddr_in（Darwin 布局：sin_len / sin_family / sin_port / sin_addr / sin_zero）
    fn put_ipv4(&mut self, ip: Ipv4Addr) {
        let mut sa = [0u8; 16];
        sa[0] = 16; // sin_len
        sa[1] = libc::AF_INET as u8; // sin_family
        sa[4..8].copy_from_slice(&u32::from(ip).to_be_bytes()); // sin_addr（网络序）
        self.ifr_ifru = sa;
    }

    fn put_flags(&mut self, f: libc::c_short) {
        let b = f.to_ne_bytes();
        self.ifr_ifru[0] = b[0];
        self.ifr_ifru[1] = b[1];
    }

    fn flags(&self) -> libc::c_short {
        libc::c_short::from_ne_bytes([self.ifr_ifru[0], self.ifr_ifru[1]])
    }

    /// ifru_mtu 是 union 开头的 int
    fn put_mtu(&mut self, mtu: i32) {
        self.ifr_ifru[0..4].copy_from_slice(&mtu.to_ne_bytes());
    }

    fn get_mtu(&self) -> i32 {
        i32::from_ne_bytes([
            self.ifr_ifru[0],
            self.ifr_ifru[1],
            self.ifr_ifru[2],
            self.ifr_ifru[3],
        ])
    }

    /// 从 union 里取出 sockaddr_in 的 sin_addr
    fn get_ipv4(&self) -> Ipv4Addr {
        Ipv4Addr::new(
            self.ifr_ifru[4],
            self.ifr_ifru[5],
            self.ifr_ifru[6],
            self.ifr_ifru[7],
        )
    }
}

/// 给点对点接口配置地址（等价 `ifconfig <if> inet <ip> <ip> netmask <mask> up`）。
pub fn configure_iface(ifname: &str, addr: &str, prefix: u8) -> Result<()> {
    // 先探测一次（幂等）：保证启动日志里一定有 [probe] 清单，
    // 哪怕 ioctl 一次成功、根本用不到这些外部命令。
    let t = tools();

    let ip_s = addr.split('/').next().unwrap_or(addr).trim();
    let ip: Ipv4Addr = ip_s.parse().with_context(|| format!("非法 IPv4: {ip_s:?}"))?;

    match configure_iface_ioctl(ifname, ip, prefix) {
        Ok(()) => {
            println!("[utun] {ifname}: {ip}/{prefix} 已生效（ioctl）");
            return Ok(());
        }
        Err(e) => println!("[utun] ioctl 配置失败，改用 ifconfig: {e:#}"),
    }

    let prog = t.ifconfig.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "配置 {ifname} 失败：ioctl 报错，且系统里找不到 ifconfig\
             （已搜索 {:?} 与 $PATH）",
            SEARCH_DIRS
        )
    })?;
    let mask = prefix_to_mask(prefix)?;
    run(prog, &[ifname, "inet", ip_s, ip_s, "netmask", &mask, "up"])
}

/// 只把接口置 UP 并设好 MTU，**暂不配地址**。
///
/// DHCP 模式（= AstralGame 的默认机制）下，地址要等 EasyTier 协商出来才能配，
/// 但 fd 注入前接口必须是 UP 的，否则 EasyTier 在 fd 上读写会失败。
pub fn prepare_iface(ifname: &str) -> Result<()> {
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            bail!("socket(AF_INET, SOCK_DGRAM) 失败: {}", std::io::Error::last_os_error());
        }
        let r = prepare_on_sock(sock, ifname);
        libc::close(sock);
        r
    }
}

fn prepare_on_sock(sock: RawFd, ifname: &str) -> Result<()> {
    unsafe {
        let mut rq = IfReq::new(ifname)?;
        rq.put_mtu(TUN_MTU);
        if let Err(e) = do_ioctl(sock, SIOCSIFMTU, &mut rq) {
            println!("[utun] 设置 MTU={TUN_MTU} 失败（大包可能不通）: {e:#}");
        }

        let mut rq = IfReq::new(ifname)?;
        do_ioctl(sock, SIOCGIFFLAGS, &mut rq).context("SIOCGIFFLAGS")?;
        rq.put_flags(rq.flags() | IFF_UP);
        do_ioctl(sock, SIOCSIFFLAGS, &mut rq).context("SIOCSIFFLAGS")?;
    }
    println!("[utun] {ifname} 已 UP（MTU {TUN_MTU}），地址等 EasyTier 协商后再配");
    Ok(())
}

fn configure_iface_ioctl(ifname: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
    let mask = prefix_to_v4(prefix)?;
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            bail!("socket(AF_INET, SOCK_DGRAM) 失败: {}", std::io::Error::last_os_error());
        }
        let r = configure_on_sock(sock, ifname, ip, mask);
        libc::close(sock);
        r
    }
}

/// 调用者在持有有效 AF_INET dgram socket 时调用。
fn configure_on_sock(sock: RawFd, ifname: &str, ip: Ipv4Addr, mask: Ipv4Addr) -> Result<()> {
    unsafe {
        // ① 本地地址 —— 必须成功
        let mut rq = IfReq::new(ifname)?;
        rq.put_ipv4(ip);
        do_ioctl(sock, SIOCSIFADDR, &mut rq).context("SIOCSIFADDR")?;

        // ② 点对点对端 —— utun 上内核一般自己派生，失败不影响收发
        let mut rq = IfReq::new(ifname)?;
        rq.put_ipv4(ip);
        if let Err(e) = do_ioctl(sock, SIOCSIFDSTADDR, &mut rq) {
            println!("[utun] 设置点对点对端失败（可忽略）: {e:#}");
        }

        // ③ 掩码 —— 失败不致命，网段路由由 add_subnet_route 补
        let mut rq = IfReq::new(ifname)?;
        rq.put_ipv4(mask);
        if let Err(e) = do_ioctl(sock, SIOCSIFNETMASK, &mut rq) {
            println!("[utun] 设置掩码失败（不致命）: {e:#}");
        }

        // ④ MTU 与 EasyTier 虚拟网对齐（放在 UP 之前，避免起来瞬间用大 MTU 收包）
        let mut rq = IfReq::new(ifname)?;
        rq.put_mtu(TUN_MTU);
        if let Err(e) = do_ioctl(sock, SIOCSIFMTU, &mut rq) {
            println!("[utun] 设置 MTU={TUN_MTU} 失败（大包可能不通）: {e:#}");
        }

        // ⑤ 置 IFF_UP —— 必须有，否则接口不工作。先读回原 flags 再或上，别冲掉别的位
        let mut rq = IfReq::new(ifname)?;
        do_ioctl(sock, SIOCGIFFLAGS, &mut rq).context("SIOCGIFFLAGS")?;
        rq.put_flags(rq.flags() | IFF_UP);
        do_ioctl(sock, SIOCSIFFLAGS, &mut rq).context("SIOCSIFFLAGS")?;

        // ⑥ 读回自证：**比对**，不是打印完就算数。
        //
        // EasyTier 内部说"分到 100.100.100.7"不算数——那只是它自己内存里的
        // 字符串。真正能说明"这个地址已经生效"的，只有从内核读回来的值。
        // 三个都必须成立：地址等于期望、接口 UP、MTU 对。
        let mut ra = IfReq::new(ifname)?;
        if libc::ioctl(sock, SIOCGIFADDR, &mut ra as *mut IfReq) < 0 {
            bail!(
                "读回 {ifname} 地址失败（配置可能未生效）: {}",
                std::io::Error::last_os_error()
            );
        }
        let got_addr = ra.get_ipv4();

        let mut rm = IfReq::new(ifname)?;
        let got_mtu = if libc::ioctl(sock, SIOCGIFMTU, &mut rm as *mut IfReq) == 0 {
            rm.get_mtu()
        } else {
            -1
        };

        let mut rf = IfReq::new(ifname)?;
        let is_up = libc::ioctl(sock, SIOCGIFFLAGS, &mut rf as *mut IfReq) == 0
            && (rf.flags() & IFF_UP) != 0;

        println!("[utun] 读回 {ifname}: addr={got_addr} mtu={got_mtu} up={is_up}");

        if got_addr != ip {
            bail!("{ifname} 地址未生效：期望 {ip}，内核实际 {got_addr}");
        }
        if !is_up {
            bail!("{ifname} 未处于 UP 状态");
        }
        if got_mtu != TUN_MTU {
            println!("[utun] 警告: MTU 期望 {TUN_MTU}，内核实际 {got_mtu}（大包可能不通）");
        }
    }
    Ok(())
}

unsafe fn do_ioctl(sock: RawFd, req: libc::c_ulong, r: &mut IfReq) -> Result<()> {
    if libc::ioctl(sock, req, r as *mut IfReq) < 0 {
        bail!("ioctl({req:#x}) 失败: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// ==================== 网段路由 ====================

/// 把虚拟网段加一条指向 utun 的路由。
///
/// 为什么不做内置实现：写 PF_ROUTE 的 RTM_ADD 需要精确复刻 `struct rt_msghdr`
/// 里 `rt_metrics` 的大小（150+ 字节，且历代 Darwin 有增删）。一旦算错，
/// 内核会从错误偏移解析 sockaddr，轻则 EINVAL，重则踩到野指针。
/// 这个代价值得不偿失——`route` 命令在所有越狱环境下都有，探测一下即可。
pub fn add_subnet_route(ifname: &str, cidr: &str) -> Result<()> {
    let t = tools();
    match t.route.as_ref() {
        Some(prog) => match run(prog, &["-n", "add", "-net", cidr, "-interface", ifname]) {
            Ok(()) => Ok(()),
            // 重连时路由已存在属于正常情况
            Err(e) if is_route_exists(&e) => {
                println!("[utun] 路由 {cidr} 已存在，跳过");
                Ok(())
            }
            Err(e) => Err(e),
        },
        None => {
            // 没有 route 命令（大部分精简越狱系统都没有）。
            // 直接走 PF_ROUTE routing socket —— /sbin/route 内部就是往这个
            // socket 写 RTM_ADD 消息，纯 syscall，root 即可，iOS 内核原生支持。
            println!("[utun] 无 route 命令，改用 PF_ROUTE 添加路由 {cidr} -> {ifname}");
            add_route_pf(ifname, cidr)
        }
    }
}

// ==================== PF_ROUTE（Darwin routing socket）====================
//
// 消息布局（64 位 Darwin，参见 xnu bsd/net/route.h）：
//   struct rt_msghdr {          // 136 字节（含 rt_metrics 12×u64=96）
//     u_short msglen;  u_char version;  u_char type;
//     u_short index;   int flags;  int addrs;  int pid;  int seq;
//     int errno;  int use;  u_int32 inits;  struct rt_metrics rmx;
//   }
//   之后依次跟 3 个 sockaddr（各按 4 字节对齐）：
//     RTA_DST(16) + RTA_GATEWAY(16, sockaddr_dl 带 if_index) + RTA_NETMASK(16)
//
// gateway 用 sockaddr_dl（链路层 + 接口号）等价于命令行的 `-interface utunX`。

const AF_ROUTE_DARWIN: libc::c_int = 17;
const AF_LINK_DARWIN: u8 = 18;
const AF_INET_DARWIN: u8 = 2;
const SOCK_RAW_DARWIN: libc::c_int = 3;
const RTM_ADD_DARWIN: u8 = 0x1;
const RTM_VERSION_DARWIN: u8 = 0x4;
const RTF_UP_DARWIN: i32 = 0x1;
const RTF_STATIC_DARWIN: i32 = 0x800;
const RTA_DST: i32 = 0x1;
const RTA_GW: i32 = 0x2;
const RTA_MASK: i32 = 0x8;
const RTMSG_HDR_LEN: usize = 136;
const SOCK_IN_LEN: usize = 16;

fn put_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_ne_bytes());
}

/// 用 PF_ROUTE routing socket 添加 `net/prefix -> ifname` 的接口路由
pub fn add_route_pf(ifname: &str, cidr: &str) -> Result<()> {
    let (net, prefix) = parse_cidr(cidr)?;
    let if_index = unsafe { libc::if_nametoindex(ifname.as_ptr() as *const libc::c_char) };
    if if_index == 0 {
        bail!("接口 {ifname} 不存在（if_nametoindex == 0）");
    }

    let mask = prefix_to_v4(prefix)?;
    let total = RTMSG_HDR_LEN + SOCK_IN_LEN * 3;
    let mut buf = vec![0u8; total];

    // rt_msghdr
    put_u16(&mut buf, 0, total as u16); // rtm_msglen
    buf[2] = RTM_VERSION_DARWIN;
    buf[3] = RTM_ADD_DARWIN;
    put_u16(&mut buf, 4, if_index as u16); // rtm_index
    put_i32(&mut buf, 8, RTF_UP_DARWIN | RTF_STATIC_DARWIN);
    put_i32(&mut buf, 12, RTA_DST | RTA_GW | RTA_MASK); // rtm_addrs
    put_i32(&mut buf, 16, unsafe { libc::getpid() } as i32); // rtm_pid
    put_i32(&mut buf, 20, 1); // rtm_seq（一次一条，固定值即可）
    // rtm_errno / rtm_use / rtm_inits / rtm_rmx 全 0

    // RTA_DST: sockaddr_in（网段地址）
    let dst = RTMSG_HDR_LEN;
    buf[dst] = SOCK_IN_LEN as u8;
    buf[dst + 1] = AF_INET_DARWIN;
    buf[dst + 4..dst + 8].copy_from_slice(&net.octets());

    // RTA_GATEWAY: sockaddr_dl（带接口号，指明走 utunX）
    let gw = dst + SOCK_IN_LEN;
    buf[gw] = SOCK_IN_LEN as u8;
    buf[gw + 1] = AF_LINK_DARWIN;
    put_u16(&mut buf, gw + 2, if_index as u16); // sdl_index

    // RTA_NETMASK: sockaddr_in
    let nm = gw + SOCK_IN_LEN;
    buf[nm] = SOCK_IN_LEN as u8;
    buf[nm + 1] = AF_INET_DARWIN;
    buf[nm + 4..nm + 8].copy_from_slice(&mask.octets());

    unsafe {
        let s = libc::socket(AF_ROUTE_DARWIN, SOCK_RAW_DARWIN, 0);
        if s < 0 {
            bail!("PF_ROUTE socket 创建失败: {}", std::io::Error::last_os_error());
        }
        let w = libc::write(s, buf.as_ptr() as *const libc::c_void, buf.len());
        let w_err = std::io::Error::last_os_error();
        libc::close(s);
        if w < 0 {
            bail!("写路由消息失败: {w_err}");
        }
    }
    println!(
        "[utun] PF_ROUTE 已提交路由: {net}/{prefix} -> {ifname} (if_index={if_index})"
    );
    Ok(())
}

fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip, p) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("CIDR 格式错误: {cidr}"))?;
    let net: Ipv4Addr = ip
        .parse()
        .map_err(|_| anyhow::anyhow!("IP 格式错误: {ip}"))?;
    let prefix: u8 = p.parse().map_err(|_| anyhow::anyhow!("前缀非法: {p}"))?;
    Ok((net, prefix))
}

fn is_route_exists(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("file exists") || s.contains("already in table")
}

// ==================== 小工具 ====================

fn prefix_to_v4(prefix: u8) -> Result<Ipv4Addr> {
    if prefix > 32 {
        bail!("前缀长度非法: {prefix}");
    }
    let bits: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Ok(Ipv4Addr::from(bits))
}

fn prefix_to_mask(prefix: u8) -> Result<String> {
    Ok(prefix_to_v4(prefix)?.to_string())
}

fn run(prog: &Path, args: &[&str]) -> Result<()> {
    println!("[utun] $ {} {}", prog.display(), args.join(" "));
    let out = Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("执行 {} 失败", prog.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "{} 退出码 {:?}: {}",
            prog.display(),
            out.status.code(),
            format!("{stderr}{stdout}").trim()
        );
    }
    Ok(())
}
