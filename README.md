# Translator — 离线实时字幕识别与翻译系统

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Android-blue)
![Version](https://img.shields.io/badge/version-1.0.0-blue)
![License](https://img.shields.io/badge/license-FSL--1.1--ALv2-blue)
![Future License](https://img.shields.io/badge/converts%20to-Apache--2.0%20after%202%20years-green)

**全离线、零网络请求的实时语音转写与字幕翻译工具**，为视频会议等实时场景优化。

- 音频不出本机：识别、翻译、字幕全部在本地完成
- 静态链接架构：推理引擎（ONNX Runtime / sherpa-onnx / Bergamot）全部编入单一核心库，无独立第三方 DLL
- © 2025–2026 MEFEason. All rights reserved.

---

## 功能特性

| 模块 | 说明 |
|------|------|
| 实时语音识别（ASR） | sherpa-onnx 流式识别，中英双语模型 |
| 离线机器翻译（MT） | Bergamot（Firefox Translations 同源引擎），英→中 |
| 悬浮字幕窗 | 实时逐行字幕，词库（glossary）可自定义术语 |
| 转写存档 | 本地记录，可导出 SRT 字幕 |
| 完全离线 | 运行期零网络请求（模型需首次导入，见下文） |

## 系统要求（Windows）

- Windows 10 及以上，64 位
- 推荐 8 GB 内存
- 无需安装 .NET / VC++ 运行库（核心库自包含）

## 安装与使用

1. 从 [Releases](../../releases) 下载最新安装包 `Translator-Setup-*.exe`
2. 运行安装（可选「为所有用户」或「仅当前用户」，后者不弹 UAC）
3. 首次启动在设置页导入模型包（`.dshm`，双语 ASR + 英中 MT）
4. 开始转写：选择音频来源（麦克风 / 系统声音回环），字幕实时浮现

> 模型权重不包含在仓库与安装包内（体积与许可原因），需在应用内导入。

## 仓库结构

```
├── translator_core/
│   ├── core/          # Rust 核心：采集/断句/识别/翻译/字幕/FFI
│   ├── ui/            # Flutter 应用（Windows / Android）
│   ├── docs/          # 设计与平台文档（DESIGN / WINDOWS_SETUP / ANDROID_SETUP）
│   └── scripts/       # 公共脚本（模型打包、离线门禁等）
├── Windows/
│   ├── scripts/       # 构建 / 签名 / 验证
│   └── src/setup.iss  # Inno Setup 安装器
├── Android/
│   └── scripts/       # Android 交叉构建
├── LICENSE            # FSL-1.1-ALv2 全文
├── NOTICE             # 第三方组件声明
├── THIRD_PARTY_LICENSES/
├── SECURITY.md        # 漏洞报告策略
└── CONTRIBUTING.md    # 贡献与 CLA
```

## 从源码构建

依赖：Rust（MSVC 工具链）、Flutter（Windows desktop）、Inno Setup 6（打安装器）。

```bash
# Windows 一键构建（Rust → Flutter → 组装 → 验证 → 安装器 → 签名）
cd Windows/scripts
TRANSLATOR_SEMVER=1.0.0 TRANSLATOR_CHANNEL=release bash package.sh

# Android
bash Android/scripts/build_android.sh
```

构建细节与排障见 [DEVELOPMENT.md](DEVELOPMENT.md)。

## 核心 FFI 接口（`translator_core.dll` / `.so`）

供宿主应用（Flutter / 其他语言）调用的 C ABI，节选：

```c
translator_create / translator_start / translator_stop   // 会话生命周期
translator_push_chunk                                   // 推送 PCM 音频块
translator_selfcheck_json                               // 自检（lib/asr/mt/glossary）
translator_session_capture_capability                   // 采集能力探测
translator_glossary_load / _reload                      // 术语词库
translator_free_string / translator_free_capture        // 内存归还
```

完整清单见 `translator_core/core/src/ffi_session.rs` 与 `translator_core/docs/DESIGN.md`。

## 许可证

本项目采用 **[FSL-1.1-ALv2](https://fsl.software/)**（Functional Source License 1.1，ALv2 Future License）。

| | 是否允许 |
|--|--|
| 个人使用 / 学习 / 研究 | ✅ 免费 |
| 公司内部使用 | ✅ 免费 |
| 基于本软件开发你自己的产品 | ✅ 免费 |
| 作为竞争性商业产品/服务对外提供 | ❌ 需商业授权 |

**每个版本自发布之日起满 2 年，自动转为 Apache License 2.0**（v1.0.0 → 2028-10-25）。

> 注意：FSL 不属于 OSI 定义的「开源许可证」，而是 Fair Source（源码可见 + 商业受限）。

商业授权联系：[2575633220@qq.com](mailto:2575633220@qq.com)

第三方组件（ONNX Runtime / sherpa-onnx / Bergamot / PCRE2 / Flutter 等）的许可与义务见 [`NOTICE`](NOTICE) 与 [`THIRD_PARTY_LICENSES/`](THIRD_PARTY_LICENSES/)。

## 贡献

提交 PR 即视为同意 [`CONTRIBUTING.md`](CONTRIBUTING.md) 中的 CLA（含重新许可授权，为 2 年后的 Apache-2.0 转换与商业分支保留可能）。

安全漏洞请按 [`SECURITY.md`](SECURITY.md) 私下报告，勿开公开 Issue。

## 致谢

[ONNX Runtime](https://github.com/microsoft/onnxruntime) ·
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) ·
[bergamot-translator](https://github.com/browsermt/bergamot-translator) ·
[Flutter](https://flutter.dev/)

---

<div align="center">

**Translator — Making conversations accessible, offline**

</div>
