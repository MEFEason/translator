# 🎤 Translator — 离线实时字幕识别与翻译系统

![Platform](https://img.shields.io/badge/platform-Windows%20|%20Android-blue)
![Version](https://img.shields.io/badge/version-1.0.0-blue)
![License](https://img.shields.io/badge/license-FSL--1.1--ALv2-blue)
![Change License](https://img.shields.io/badge/after%202%20years-Apache--2.0-green)
![Static Linking](https://img.shields.io/badge/static--linking--ORT%2Fsherpa--green)

> 🚀 **全离线、无网络依赖的实时字幕识别与翻译系统**，专为视频会议场景打造。
> **© 2025~2026 MEFEason. All rights reserved.**

---

## 📖 项目简介

Translator 是一款**完全离线**的桌面应用程序，提供实时音频转写、多语言字幕翻译和会议记录生成功能。无需网络连接，保护隐私的同时保证低延迟体验。

### ✨ 核心特性

- 🔇 **实时语音识别 (ASR)** — 支持多语言说话人分离
- 🌐 **机器翻译 (MT)** — 内置多种翻译模型
- 📝 **字幕生成** — 实时生成 SRT 格式字幕文件
- 🎯 **静态链接架构** — 无任何第三方 DLL 依赖，规避安全软件误报
- 💾 **完全离线** — 零网络请求，保护隐私与数据安全

---

## 🖥️ 系统要求

### Windows

| 组件 | 最低配置 | 推荐配置 |
|------|---------|---------|
| **操作系统** | Windows 10/11 64-bit | Windows 11 24H2 |
| **CPU** | 双核 2.0 GHz | 四核 3.5+ GHz |
| **内存** | 8 GB | 16 GB+ |
| **磁盘** | 5 GB 可用空间 | 20 GB+ (含模型缓存) |
| **.NET** | .NET 6 Runtime | — |

### Android

| 组件 | 版本 |
|------|------|
| **Android Studio** | 2026.1.4+ |
| **JDK** | OpenJDK 25 |
| **SDK/NDK** | API 34+, NDK r29+ |

---

## 🚀 快速开始

### Windows 安装（推荐方式）

```bash
# 下载官方安装包
# 位置：dist/release-v1.0.0-build2026.1025.20.45.exe

# 或从 outputs 目录直接运行
cd Translator/Windows/outputs
.\translator.exe
```

### 命令行启动

```powershell
# Windows
cd Translator/Windows/outputs
.\translator.exe --help

# 指定模型路径
.\translator.exe -m models/asr_en.onnx -mt models/translate_zh_en.onnx
```

---

## 📦 产品架构

### 目录结构

```
Translator/
├── Windows/                  # Windows 平台目录
│   ├── outputs/             # ✅ 当前发布版本 (37 文件，~504 MB)
│   │   ├── translator.exe   # 主程序入口 (99 KB)
│   │   ├── translator_core.dll  # Rust 核心库 (28.7 MB, 静态链接)
│   │   ├── flutter_windows.dll    # Flutter 引擎 (21.3 MB)
│   │   ├── data/            # Flutter 资源文件
│   │   ├── models/          # 离线 ASR/翻译模型 (~94 MB)
│   │   ├── SHA256SUMS.txt   # 完整性校验和
│   │   └── version.ini      # 版本信息
│   ├── scripts/             # 打包与签名脚本 (19 个文件)
│   └── setup.iss            # Inno Setup 安装器源文件
│
├── dist/                    # 已签名的安装包
│   ├── beta-v0.9.x/        # 测试版历史版本 (v0.9.1 ~ v0.9.52)
│   └── release-v1.0.0-*.exe  # 正式版安装包
│
├── Android/                 # Android 平台目录
│   ├── src/                # Flutter + Kotlin 源码
│   └── scripts/            # Android 打包脚本 (3 个文件)
│
├── translator_core/        # Rust 核心代码库
│   ├── src/                # C++ 核心实现
│   ├── scripts/            # 验证与构建脚本
│   └── docs/               # API 文档与指南
│
└── tools/                  # 开发工具集
    ├── innosetup/          # Inno Setup 6.7.3 (ISCC.exe)
    └── 7z/                 # 压缩工具
```

---

## 🔬 技术亮点

### 静态链接策略

```ini
[version]
full = release-v1.0.0-build2026.1025.20.45
channel = release
semver = 1.0.0
num = 1.0.0.1025
```

- ✅ **零第三方 DLL** — 磁盘上不存在 `onnxruntime.dll` / `sherpa-onnx-*.dll`
- ✅ **能力内联** — ONNX Runtime + Sherpa-onnx + Bergamot 全部静态链接进 `translator_core.dll`
- ✅ **规避误报** — 避免"新增/修改系统 DLL"的安全软件误报问题

### 版本管理

- **语义化版本**: `{channel}-v{主}.{次}.{修订}-build{YYYY}.{MMDD}.{HH}.{mm}`
- **目录式发布**: 多版本原子共存，支持即时切换回滚
- **完整追溯**: 每次构建生成独立版本目录，永不覆盖历史

### 构建流程

```bash
# Windows 全量构建 (Rust → Flutter → 组装 → 签名 → 验证)
TRANSLATOR_SEMVER=1.0.0 TRANSLATOR_CHANNEL=release bash Windows/scripts/package.sh

# Android 构建
bash Android/build_android.sh
```

---

## 🔐 安全与合规

| 检查项 | 状态 | 说明 |
|--------|------|------|
| **离线运行** | ✅ | 零网络请求，保护隐私 |
| **静态链接** | ✅ | 无第三方 DLL 依赖 |
| **签名验证** | ✅ | SHA256 完整校验 |
| **自检脚本** | ✅ | FFI 端到端验证 + 离线性门禁 |

### 验证命令

```bash
# Windows 平台验证
cd Translator/Windows/outputs
find . -type f ! -name SHA256SUMS.txt -exec sha256sum {} \; > SHA256SUMS.txt

# FFI 端对端验证 (需真实后端)
python translator_core/scripts/verify_ffi.py Translator 5

# 离线性门禁
python translator_core/scripts/check_offline.py
```

---

## 🛠️ 开发指南

### Windows 构建环境

| 工具 | 版本 | 位置 |
|------|------|------|
| **Rust** | stable-x86_64-pc-windows-msvc | `~/.cargo` |
| **Flutter** | 3.x+ | 可选，交叉编译用 |
| **Inno Setup** | 6.7.3 | `tools/innosetup/ISCC.exe` |

### Android 构建环境

| 工具 | 版本 | 位置 |
|------|------|------|
| **Android Studio** | 2026.1.4+ | `C:\Program Files\Android\Android Studio` |
| **JDK (JBR)** | OpenJDK 25 | `Android Studio/jbr/` |
| **Android SDK** | API 34+ | `C:\Program Files (x86)\Android\android-sdk` |
| **Android NDK** | r29+ | `C:\Program Files (x86)\Android\AndroidNDK` |

### Android CLI（新版）

```bash
SDK="C:/Program Files (x86)/Android/android-sdk"
"$SDK/cmdline-tools/latest/bin/android.exe" --sdk="$SDK" sdk list
```

---

## 📚 API 参考

### FFI 接口

```rust
// ASR 识别
ffi_asr_init("models/asr_model.onnx");
ffi_asr_transcribe(&audio_path, &result);

// 翻译服务
ffi_mt_init("models/translate_model.onnx");
ffi_mt_translate(&source_text, "en", "zh");
```

### 详细文档

- [Windows 构建指南](translator_core/docs/WINDOWS_SETUP.md)
- [API 参考](translator_core/docs/api_reference.md)
- [FFI 开发](translator_core/docs/ffi_guide.md)

---

## 📝 版本历史

### v1.0.0 (2026-10-25)

**里程碑发布**: 正式版 v1.0.0，完整功能与优化

- ✅ **静态链接架构完成** — ORT + Sherpa + Bergamot 全部内联
- ✅ **无第三方 DLL** — 规避安全软件误报
- ✅ **完整验证流程** — SHA256 校验 + FFI 端到端 + 离线性门禁
- ✅ **多平台支持** — Windows 桌面 + Android

### v0.9.x (Beta)

- 🚀 `v0.9.52` — 最后一次测试版快照
- 📈 `v0.9.1` ~ `v0.9.45` — 功能迭代与性能优化

---

## ⚖️ License

本项目采用 **[Functional Source License, Version 1.1, ALv2 Future License](https://fsl.software/FSL-1.1-ALv2.template.md)**（简称 `FSL-1.1-ALv2`），属 **Fair Source（公平源码）** 阵营。

### 你可以做什么 ✅

| 场景 | 是否免费 |
|------|---------|
| 个人下载使用 | ✅ 免费 |
| 学习、研究、非商业用途 | ✅ 免费 |
| 公司**内部**使用 | ✅ 免费 |
| 在本项目基础上开发**你自己的**产品 | ✅ 免费 |
| 为被许可方提供专业服务 | ✅ 免费 |

### 你不能做什么 ❌

- 把本软件（或其功能相同/实质相似的变体）作为**商业产品或服务**提供给第三方
  —— 这是 FSL 定义的 **Competing Use（竞争性用途）**，禁止

### 2 年后自动开源 ✅

自本软件每个版本发布之日起**满 2 年**，该版本自动转为 **Apache License 2.0**，
无需申请、无需额外授权。届时任何人可自由使用、修改、商用。

### 商业授权 💼

需要 Competing Use 场景（把本软件做成商业服务对外提供）的，请联系
[2575633220@qq.com](mailto:2575633220@qq.com) 获取商业许可。

### 第三方组件

本项目静态链接了 ONNX Runtime（MIT）、sherpa-onnx（Apache-2.0）、
Bergamot（MPL-2.0）等组件，其条款独立适用。
完整清单见 [`NOTICE`](./NOTICE)，许可原文见 [`THIRD_PARTY_LICENSES/`](./THIRD_PARTY_LICENSES/)。

> ⚠️ FSL **不是** OSI 认可的开源许可证（与 MIT / Apache-2.0 / GPL 不同），
> 而是「源码可见 + 商业受限」。若你需要 OSI 认证的开源许可，请等待 2 年后
> 的 Apache-2.0 自动转换，或联系我们提供其他授权方案。

---

## 🤝 贡献指南（Contributing）

提交 PR 即表示你同意：

1. 你的贡献以**与本项目相同的许可证**（FSL-1.1-ALv2）授权；
2. 你授予项目维护者**重新许可（re-license）**你贡献的权利，
   以便未来变更项目许可证（如 2 年后转 Apache-2.0，或商业授权分支）。

> 📌 **这一步必须在接受第一个 PR 之前完成**，否则日后变更许可证需征得所有贡献者同意。

---

## 📮 联系方式

- **🐱 GitHub 仓库**: [MEFEason/translator](https://github.com/MEFEason/translator)
- **💬 问题反馈**: Issues & Pull Requests welcome
- **📧 联系邮箱**: [2575633220@qq.com](mailto:2575633220@qq.com)（在线时间不等）
- **💼 商业授权**: 同上邮箱
- **©️ 版权所有**: © 2025~2026 MEFEason. All rights reserved.

---

## ⚠️ 注意事项

1. **离线运行** — 所有功能均需本地模型支持，确保网络环境稳定时下载资源
2. **磁盘空间** — 每次构建生成独立版本目录 (~500 MB/版本)，请确保充足空间
3. **管理员权限** — Windows 安装需要 UAC 提升（如安装到 `Program Files`）

---

## 🙏 致谢

感谢以下开源项目：

- [ONNX Runtime](https://github.com/microsoft/onnxruntime)
- [Sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)
- [Bergamot](https://github.com/k2-fsa/bergamot)
- [Flutter](https://flutter.dev/)

---

<div align="center">
  <strong>Translator — Making conversations accessible, offline</strong>
</div>
