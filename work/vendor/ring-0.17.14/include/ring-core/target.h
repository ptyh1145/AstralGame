// Copyright 2023 The BoringSSL Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#ifndef OPENSSL_HEADER_TARGET_H
#define OPENSSL_HEADER_TARGET_H

// Preprocessor symbols that define the target platform.
//
// This file may be included in C, C++, and assembler and must be compatible
// with each environment. It is separated out only to share code between
// <ring-core/base.h> and <ring-core/asm_base.h>. Prefer to include those headers
// instead.

#if defined(__x86_64) || defined(_M_AMD64) || defined(_M_X64)
#define OPENSSL_64_BIT
#define OPENSSL_X86_64
#elif defined(__x86) || defined(__i386) || defined(__i386__) || defined(_M_IX86)
#define OPENSSL_32_BIT
#define OPENSSL_X86
#elif defined(__AARCH64EL__) || defined(_M_ARM64)
#define OPENSSL_64_BIT
#define OPENSSL_AARCH64
// [astral-ios6] 新版 clang 对 32 位 ARM（含 iOS 的 armv7s）不再定义
// 古老的 GCC 兼容宏 __ARMEL__，只定义 __arm__ / __ARM_ARCH。
// 原判定只看 __ARMEL__，于是 iOS 上 OPENSSL_ARM 没被定义：
//   * crypto/*.c 里的 neon 分支不编译
//   * 所有 .S 被 `#if ... defined(OPENSSL_ARM)` 整段预处理掉 → 空 .o
//     （表现成 ranlib 的 "has no symbols" 和 17 个 _ring_core_* 未定义）
// 加上 __arm__ 兜底即可。
#elif defined(__ARMEL__) || defined(_M_ARM) || defined(__arm__)
#define OPENSSL_32_BIT
#define OPENSSL_ARM
// All of following architectures are only supported when `__BYTE_ORDER__` can be used to detect
// endianness (in crypto/internal.h).
#elif !defined(__BYTE_ORDER__)
#error "Cannot determine endianness because __BYTE_ORDER__ is not defined"
// Targets are assumed to be little-endian unless __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__.
#elif !(defined(__ORDER_LITTLE_ENDIAN__) && (__BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__)) && \
      !(defined(__ORDER_BIG_ENDIAN__) && (__BYTE_ORDER__ == __ORDER_BIG_ENDIAN__))
#error "Unsupported endianness"
#elif defined(__LP64__)
#define OPENSSL_64_BIT
#elif defined(__ILP32__)
#define OPENSSL_32_BIT
// Versions of GCC before 10.0 didn't define `__ILP32__` for all 32-bit targets.
#elif defined(__MIPSEL__) || defined(__MIPSEB__) || defined(__PPC__) || defined(__powerpc__) || defined(__csky__) || defined(__XTENSA__)
#define OPENSSL_32_BIT
#else
#error "Unknown target CPU"
#endif

#if defined(__APPLE__)
#define OPENSSL_APPLE
#endif

#if defined(_WIN32)
#define OPENSSL_WINDOWS
#endif

#if defined(__has_feature)
#if __has_feature(address_sanitizer)
#define OPENSSL_ASAN
#endif
#if __has_feature(thread_sanitizer)
#define OPENSSL_TSAN
#endif
#if __has_feature(memory_sanitizer)
#define OPENSSL_MSAN
#define OPENSSL_ASM_INCOMPATIBLE
#endif
#if __has_feature(hwaddress_sanitizer)
#define OPENSSL_HWASAN
#endif
#endif

// [astral-ios6] 上游在这里**无条件禁用 Apple 平台的 32 位 ARM 汇编**：
//
//     // Disable 32-bit Arm assembly on Apple platforms. The last iOS
//     // version that supported 32-bit Arm was iOS 10.
//     #if defined(OPENSSL_APPLE) && defined(OPENSSL_ARM)
//     #define OPENSSL_ASM_INCOMPATIBLE
//     #endif
//
// 而我们的目标恰恰是 iOS 8/9 的 armv7s，需要这些汇编。被禁用后
// OPENSSL_NO_ASM 会被定义，所有 .S 被预处理成空文件，最终表现为
//   ranlib: file: libring_core_*.a(xxx.o) has no symbols
// 以及链接期 17 个 _ring_core_*_neon / _nohw 符号 undefined。
// 所以这里解除禁用。
#if defined(OPENSSL_APPLE) && defined(OPENSSL_ARM) && defined(RING_APPLE_ARM_NO_ASM)
#define OPENSSL_ASM_INCOMPATIBLE
#endif

#if defined(OPENSSL_ASM_INCOMPATIBLE)
#undef OPENSSL_ASM_INCOMPATIBLE
#if !defined(OPENSSL_NO_ASM)
#define OPENSSL_NO_ASM
#endif
#endif  // OPENSSL_ASM_INCOMPATIBLE

#if !defined(OPENSSL_X86_64) && !defined(OPENSSL_AARCH64)
#define OPENSSL_SMALL
#endif

#endif  // OPENSSL_HEADER_TARGET_H
