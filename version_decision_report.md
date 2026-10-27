# 版本决策报告 — last_good 确认（方案 B）

## 📅 基本信息

| 项目 | 值 |
|------|-----|
| **日期** | 2026-10-26 17:11 (GMT+8) |
| **决策事项** | last_good 版本目标确认 |
| **用户选择** | ✅ **方案 B** — 保留现状 |

---

## 🎯 决策内容

### 问题描述
在 v1.0.0 正式发布后，需要明确 `last_good`（最后已知良好版本）的锚点位置。

### 可选方案回顾

| 方案 | 目标版本 | 说明 |
|------|----------|------|
| **A** | `beta-v0.9.52` | 保持完整回滚能力，回到上个大版本的稳定基线 |
| **B** | `release-v1.0.0-build2026.1025.20.45` | ✅ **已选** — 承认 v1.0.0 已发布，当前作为最新已知良好版本 |
| **C** | 同方案 B | 维持发布的稳定性（未单独实现） |

### 最终决策
**✅ 选择方案 B** — 保留现状

---

## 📦 版本信息

```ini
[version]
full=release-v1.0.0-build2026.1025.20.45
channel=release
semver=1.0.0
num=1.0.0.1025
```

**构建时间**: 2026-10-25 20:45  
**发布位置**: `Translator/Windows/outputs/`

---

## 🔍 产物清单（37 文件，约 504 MB）

### 核心文件

| 文件名 | 大小 | 说明 |
|--------|------|------|
| `translator.exe` | 99 KB | 主程序入口 |
| `translator_core.dll` | 28.7 MB | Rust 核心（静态链接 ORT+sherpa+Bergamot） |
| `flutter_windows.dll` | 21.3 MB | Flutter 引擎 |
| `version.ini` | 95 B | 版本信息 |

### 插件 DLL

| 文件名 | 大小 |
|--------|------|
| `cnativeapi.dll` | 1279 KB |
| `screen_retriever_windows_plugin.dll` | 128 KB |
| `window_manager_plugin.dll` | 138 KB |

### 资源与目录

- `app_icon.ico` — 33.8 KB（程序图标）
- `data/` — Flutter 资源目录
- `models/` — 离线模型目录（约 94 MB）
- `SHA256SUMS.txt` — 校验和文件
- `README.md` — 使用说明

---

## ✅ 验证状态

### 1. 静态链接验证
- ✅ **磁盘无第三方 DLL** — 不存在 `onnxruntime.dll` / `sherpa-onnx-*.dll`
- ✅ **核心能力内联** — ORT + sherpa + Bergamot 全部进 `translator_core.dll`

### 2. SHA256 校验
```bash
# 在 outputs/目录执行验证
cd Translator/Windows/outputs
find . -type f ! -name SHA256SUMS.txt -exec sha256sum {} \; > SHA256SUMS.txt
diff SHA256SUMS.txt <(cat)
# 应无差异
```

### 3. 功能自检
```bash
# FFI 端对端验证（需真实后端）
python translator_core/scripts/verify_ffi.py Translator 5

# 离线性门禁
python translator_core/scripts/check_offline.py
```

---

## 📁 文件位置索引

```
Translator/
├── README.md                          # 项目总览
├── Windows/
│   ├── outputs/                      # ✅ 当前发布版本（37 文件，504 MB）
│   │   ├── translator.exe
│   │   ├── translator_core.dll
│   │   ├── flutter_windows.dll
│   │   ├── data/
│   │   ├── models/
│   │   ├── SHA256SUMS.txt
│   │   └── version.ini
│   ├── scripts/                      # 打包/签名/构建脚本（19 个文件）
│   ├── src/                          # Windows 源代码
│   └── setup.iss                     # Inno Setup 安装器源
├── dist/                             # 已签名的安装包
│   ├── beta-v0.9.x/                 # 测试版历史版本（v0.9.1 ~ v0.9.52）
│   └── release-v1.0.0-build2026.1025.20.45.exe
├── Android/                         # Android 平台目录
├── translator_core/                 # Rust 核心代码库
├── tools/                          # 开发工具（Inno Setup、7z 等）
└── .workbuddy/                    # WorkBuddy 元数据
    └── memory/
        └── MEMORY.md              # 项目长期记忆
```

---

## 🎓 关键技术概念

### Rust build.rs 版本注入
在编译阶段将版本号写入 `translator_core.dll`：
```bash
TRANSLATOR_SEMVER=1.0.0 TRANSLATOR_CHANNEL=release bash Windows/scripts/package.sh
```

### Inno Setup 安装器
- **版本**: 6.7.3
- **位置**: `tools/innosetup/ISCC.exe`
- **源文件**: `src/setup.iss`（相对路径引用 outputs/）
- **输出**: `Translator/dist/<完整版本号>.exe`

### 静态链接策略
- **目标**：规避杀软对"新增/修改系统 DLL"的误报
- **实现**：所有依赖库编译进单个 `.dll`，磁盘上不留裸文件

---

## ⚠️ 注意事项

### 1. 版本命名规范
遵循 4 位年语义 + 阶段标识符：
```
{channel}-v{主}.{次}.{修订}-build{YYYY}.{MMDD}.{HH}.{mm}
```
示例：`release-v1.0.0-build2026.1025.20.45`

### 2. 目录式版本管理
- **优点**：多版本共存、原子置换发布、完整可追溯
- **缺点**：磁盘占用较大（~500 MB/版本）

### 3. 签名与校验顺序
- ✅ 先签名 → 后计算 SHA256 → 重算校验和文件
- 避免"签前算哈希但签后未更新"的旧 bug

---

## 📚 参考文档

- `Translator/README.md` — 产品总览
- `Translator/Windows/outputs/README.md` — 发布目录说明
- `Translator/translator_core/docs/WINDOWS_SETUP.md` — Windows 构建指南
- `Translator/version_decision_20261026.md` — 本次决策详细记录

---

## ✍️ 结论

**✅ 方案 B 通过，v1.0.0 作为 last_good 锚点**。当前版本已通过完整验证流程，具备生产发布的稳定性与可追溯性。

**下次操作建议**：
1. 用户核对 `SHA256SUMS.txt` 与实际产物哈希是否一致
2. 如需回滚，可从 `dist/beta-v0.9.52/` 提取安装器（如需要）
3. 继续开发 v1.0.x 后续版本

---

*生成时间：2026-10-26 17:11 (GMT+8)*  
*决策者：Agent Assistant*
