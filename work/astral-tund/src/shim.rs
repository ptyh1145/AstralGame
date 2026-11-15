//! iOS 兼容 shim：补上标准库引用、但老系统缺的少数符号。
//!
//! 背景：rustc 1.102 把 `armv7s-apple-ios` 的最低部署目标硬编码成 10.0，
//! 标准库按 iOS 10 的 API 面编译，于是引用了这些符号：
//!
//! | 符号 | 引入版本 | 本文件处理 |
//! |---|---|---|
//! | `clock_gettime` | iOS 10 | **在这里实现**（用 gettimeofday 等价替代） |
//! | `__tlv_bootstrap` | iOS 8 | 链接期 `-Wl,-U` 放行（iOS 8+ dyld 原生提供） |
//! | `__tlv_atexit` | iOS 8 | 同上 |
//! | `openat`/`fstatat`/`renameat`/`unlinkat` | iOS 8 | 同上 |
//! | `_CCRandomGenerateBytes` | iOS 2 | 同上（6.1 SDK 的 tbd 没列，实际在 libcommonCrypto） |
//!
//! 也就是说：**iOS 6/7 需要更重的兼容层（TLS 要自己用 pthread_key 模拟）**，
//! 而 iOS 8+ 只差 `clock_gettime` 这一个，所以先做 iOS 8 版本。

use libc::{c_int, c_void, size_t, timespec, timeval};

const CLOCK_REALTIME: c_int = 0;
const CLOCK_MONOTONIC: c_int = 6;

/// 用 `gettimeofday` 实现 `clock_gettime`。
///
/// std 的 `Timespec::now` 和 quanta 的 `Clock::now` 都会调用它；
/// iOS 9 及以下的 libSystem 没有这个符号，不补的话 dyld 解析阶段就失败
/// （表现为一启动就崩，而不是运行时才出错）。
///
/// `CLOCK_MONOTONIC` 这里也用系统时间近似：不保证严格单调，
/// 但 daemon 只用它做超时/耗时统计，NTP 的微调不影响功能正确性。
#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clk_id: c_int, tp: *mut timespec) -> c_int {
    if tp.is_null() {
        return -1;
    }
    if clk_id != CLOCK_REALTIME && clk_id != CLOCK_MONOTONIC {
        // CLOCK_PROCESS_CPUTIME_ID 等其它时钟不支持
        return -1;
    }
    let mut tv: timeval = std::mem::zeroed();
    if libc::gettimeofday(&mut tv, std::ptr::null_mut()) != 0 {
        return -1;
    }
    (*tp).tv_sec = tv.tv_sec as libc::time_t;
    (*tp).tv_nsec = (tv.tv_usec as libc::c_long) * 1000;
    0
}

/// 用 `arc4random_buf`（libSystem，iOS 2+ 就有）替代 `CCRandomGenerateBytes`。
///
/// std 的 `hashmap_random_keys` 用它给 HashMap 取随机种子。
/// 它本来在 `libcommonCrypto.dylib` 里，但 6.1 SDK 的 tbd 没列这个符号；
/// 与其额外链接一个库，不如直接用 libSystem 的 arc4random —— 语义等价。
///
/// 返回值沿用 CommonCrypto 的约定：0 表示成功。
#[no_mangle]
pub unsafe extern "C" fn CCRandomGenerateBytes(bytes: *mut c_void, count: size_t) -> c_int {
    if bytes.is_null() {
        return -1;
    }
    libc::arc4random_buf(bytes, count);
    0
}