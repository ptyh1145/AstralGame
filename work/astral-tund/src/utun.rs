//! iOS / macOS 的 utun 设备（PF_SYSTEM + com.apple.net.utun_control）。
//!
//! iOS 6 上没有 NetworkExtension（iOS 8+），所以拿不到 VpnService 那种 fd；
//! 但内核仍然提供 macOS 同款的 utun 控制通道，**root 进程**可以自己开一个。
//! 越狱设备上 daemon 以 root 运行，即可用本模块拿到 fd 交给 EasyTier。
//!
//! 报文格式（关键，与 EasyTier 的 iOS 分支一致）：
//!   read()  -> [4 字节 AF 头][IP 包]      （头里是 AF_INET/AF_INET6，大端 u32）
//!   write() -> 必须自己补上这 4 字节头
//! EasyTier 在 `create_dev_for_mobile` 里对 iOS 设了 `packet_information(false)`
//! 并手工补头（proto = PF_INET/PF_INET6），正好就是 utun 的约定，
//! 所以我们直接把裸 utun fd 喂给它即可，**核心代码无需改动**。

#![allow(non_camel_case_types)]

use anyhow::{bail, Context, Result};
use std::os::fd::RawFd;
use std::process::Command;

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

/// 给点对点接口配置地址（darwin 用两地址形式）。
/// 例：ifconfig utun0 inet 10.144.144.5 10.144.144.5 netmask 255.255.255.0 up
pub fn configure_iface(ifname: &str, addr: &str, prefix: u8) -> Result<()> {
    let mask = prefix_to_mask(prefix)?;
    let ip = addr.split('/').next().unwrap_or(addr);
    run("/sbin/ifconfig", &[ifname, "inet", ip, ip, "netmask", &mask, "up"])?;
    Ok(())
}

/// 把虚拟网段加一条指向 utun 的路由。
pub fn add_subnet_route(ifname: &str, cidr: &str) -> Result<()> {
    run("/sbin/route", &["-n", "add", "-net", cidr, "-interface", ifname])?;
    Ok(())
}

fn prefix_to_mask(prefix: u8) -> Result<String> {
    if prefix > 32 {
        bail!("前缀长度非法: {prefix}");
    }
    let bits: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Ok(format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xff,
        (bits >> 16) & 0xff,
        (bits >> 8) & 0xff,
        bits & 0xff
    ))
}

fn run(prog: &str, args: &[&str]) -> Result<()> {
    println!("[utun] $ {} {}", prog, args.join(" "));
    let out = Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("执行 {prog} 失败"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{prog} 退出码 {:?}: {}", out.status.code(), stderr.trim());
    }
    Ok(())
}
