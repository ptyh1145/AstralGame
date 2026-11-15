# Astral Game → iOS 6（overlay）

把本目录内容**原样覆盖到 AstralGame 仓库根目录**，然后 push：

```
.github/workflows/build-ios6.yml    # CI：macos-14 + 自备 iOS 6.1 SDK + nightly build-std
sdk/iPhoneOS6.1.sdk.tar.gz          # 3.8MB，已校验：含 armv7 且是 iOS 6 的真实导出表
work/astral-tund/                   # 守护进程（utun + EasyTier + fd 注入）
work/ios6/build-ci.sh               # 构建脚本（本地 Mac 也能直接跑）
work/ios6/README.md                 # 背景与踩坑说明
```

## 触发

- Actions → `build-ios6` → Run workflow
- 或本机：`REPO_ROOT=$PWD ./work/ios6/build-ci.sh`

产物：`dist/astral-tund`（armv7，已 `ldid -S` 签名）。

## SDK 已完成的校验

| 项 | 结果 |
|---|---|
| `libSystem.tbd` archs | 含 `armv7` ✓ |
| `libsystem_m.tbd` | 有 `_sin`/`_cos`，**无** `___sincos_stret`（iOS 7+）✓ |
| `libobjc.tbd` | 有 `_objc_msgSend`，**无** `_objc_readClassPair`（iOS 8+）✓ |
| `libdispatch.tbd` | 有 `_dispatch_once`，**无** `_dispatch_activate`（iOS 10+）✓ |

这三条"没有"很关键：它保证**链接期就拦住 iOS 7+ 符号**，不会出现
"链接通过、真机 Symbol not found"。

## SDK 是否入库

`sdk/` 约 25MB，单文件远小于 GitHub 100MB 限制，可直接 commit。
若不想入库，改用 secret `IOS6_SDK_URL` 指向 tar.gz（workflow 已支持）。
