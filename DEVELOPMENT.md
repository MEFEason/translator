# Translator — 开发文档（构建 / 验证 / 排障）

> 本文档面向贡献者与构建者，描述如何从源码构建与验证本产品。
> 普通使用者请阅读 [README.md](README.md)。

完全离线的实时语音转写 + 翻译字幕工具。ASR（sherpa-ONNX）与 MT（Bergamot）
的推理代码**全部静态链接进 `translator_core.dll`**，磁盘上不存在任何
`onnxruntime.dll` / `sherpa-onnx-*.dll`，从根本上规避 360 对「新增系统 DLL」的误报。

> **许可证**：FSL-1.1-ALv2（Functional Source License 1.1，2 年后自动转 Apache-2.0）
> 详见 `LICENSE`；第三方组件声明见 `NOTICE`。

---

## 一、构建（唯一入口）

**所有构建都必须走 `package.sh`** —— 它注入预编译库路径、注入版本号、
并在验证通过后才覆盖 `last_good` / `outputs`。

```bash
cd Translator/translator_core

# 测试版（正式发布用 release 通道）
TRANSLATOR_SEMVER=0.9.27 TRANSLATOR_CHANNEL=beta   bash Windows/scripts/package.sh

# 正式版
TRANSLATOR_SEMVER=1.0.0  TRANSLATOR_CHANNEL=release bash Windows/scripts/package.sh

# 只重新打包（复用已构建产物，跳过编译）
SKIP_BUILD=1 bash Windows/scripts/package.sh
```

> ⚠️ **环境变量名是 `TRANSLATOR_SEMVER` / `TRANSLATOR_CHANNEL`**（不是 `DSH_*`）。
> 缺省会回落到 `snapshot` / `0.0.0`，并把版本串写进 DLL 与 `version.ini`。

`package.sh` **一次跑完所有环节**，不需要再手动编译安装器：

| 阶段 | 内容 |
|---|---|
| 0 | 前置检查（去重代理环境变量大小写冲突） |
| 0.5 | 注入 `BERGAMOT_PREBUILT` / `BERGAMOT_SRC_DIR` / `SHERPA_ONNX_LIB_DIR` |
| 1 | `cargo build --release`（Rust 源码未变则跳过）+ `flutter build windows --release` |
| 2 | 组装 staging（收集全部 `*_plugin.dll`，剔除 ORT/sherpa 黑名单） |
| 5 | **自动编译安装器**（找 `ISCC.exe` → 编译 `src/setup.iss` → 签名）到 `dist/` |
| 3 | 验证 staging |
| 3.2 | **出厂门禁 `verify_product.py`**（失败则 `die`，不动 `last_good`） |
| 3.5 | 代码签名（360 QVM 降误报） |
| 4 | 备份旧产物 → `last_good`，新产物 → `outputs` |

### 手动跑 cargo build 的必要环境变量

**不走 `package.sh` 直接 `cargo build --release` 必须手动带这三个变量**，
否则会触发 Bergamot 的全新 CMake 配置 → 本机 CMake 4.3.1 拒绝子项目旧 policy → panic：

```bash
export BERGAMOT_PREBUILT=... BERGAMOT_SRC_DIR=... SHERPA_ONNX_LIB_DIR=...
```

（具体路径见 `package.sh` 第 149~170 行的 export 段。）

### 增量构建

Rust release 模式默认 `CARGO_INCREMENTAL=0`，`package.sh` 会打开增量。
Rust 源码未变且版本戳未变时**跳过 cargo build**（全量 5-10 分钟 → 几十秒）。
跳过时 `.version.ini` 用**从 DLL 反读的真实版本串**，保证
「关于页 / 安装目录名 / version.ini」三处一致。

---

## 二、验证与测试脚本

| 脚本 | 用途 | 命令 |
|---|---|---|
| `Windows/scripts/verify_product.py` | **出厂门禁**（音频无关）：加载真实 DLL + 真实模型，检查自检 `lib/asr/mt/glossary` 全 `ok` | `python Windows/scripts/verify_product.py <DLL目录>` |
| `scripts/check_offline.py` | 离线性门禁（三层：源码审计 / 运行期依赖断言 / 构建期白名单） | `python scripts/check_offline.py` |
| `scripts/verify_ffi.py` | 模拟 Dart 侧完整 FFI 调用路径（多声源、转写盘、历史回溯） | `python scripts/verify_ffi.py Translator 5` |
| `scripts/selftest.sh` | 流水线自检（采集→断句→识别→翻译→字幕），**改完 core/ 后最快的反馈回路** | `bash scripts/selftest.sh` |
| `scripts/selftest.sh --asr-sherpa --mt-bergamot --seconds 12 --bench` | 真实后端跑通整条链路 | 同上 |
| `scripts/test_model_pack.py` | 模型包单元测试 | `python scripts/test_model_pack.py` |

> ⚠️ **`verify_product.py` 是门禁，不是日志**。它真的拦下过缺陷（JSON 双重转义），
> 别把它降级成「只打印结果」。

---

## 三、辅助脚本（按需使用，非构建必需）

| 脚本 | 用途 |
|---|---|
| `Windows/scripts/build_bergamot.py` | 编译 Bergamot 静态库（产物缺失时先跑这个） |
| `Windows/scripts/build_windows.sh` | 仅编译 Windows Rust 核心 → `ui/windows/rust/translator_core.dll` |
| `Android/scripts/build_android.sh` | 交叉编译 Android → `libtranslator_core.so`（仅 arm64） |
| `scripts/build_model_pack.py` | 打包模型 |
| `scripts/fetch_models.py` / `fetch_bergamot.py` | 下载模型 / Bergamot 资源（**唯一允许联网的脚本**，仅构建期） |
| `scripts/setup_flutter.sh` | 安装 Flutter SDK（国内镜像） |
| `Android/scripts/update_sdk_paths.sh` | Android SDK 迁移到 D 盘后改路径（**一次性迁移脚本，已跑过，日常不用**） |
| `Android/scripts/check_android_env.py` | Android 环境自检 |
| `scripts/gen_test_wav.py` / `gen_tts_wav.ps1` | 生成测试音频 |
| `Windows/scripts/pe_version.py` | 写 PE 版本资源 |
| `Windows/scripts/net_probe.py` | 网络通道探测（**只读**系统代理，实测哪条通道真正可用） |
| `Windows/scripts/git_net.sh` | git 网络自动适配包装器（探测 → 只对本次命令生效，不改任何配置） |
| `Windows/scripts/copy_ort.py` / `copy_sherpa_dlls.ps1` | 拷贝 ORT/sherpa DLL（**static 模式下已不需要**） |
| `Windows/scripts/patch_protobuf_ns.py` | protobuf namespace 修补 |
| `Windows/scripts/windows_plugin_junctions.ps1` | 建 Windows 插件目录 junction（`package.sh` 会自动调用） |
| `Windows/scripts/cert_tools.ps1` / `_renew_cert.ps1` | 代码签名证书工具（`_renew_cert.ps1` 生成 `codesign.pfx`） |
| `Windows/scripts/sign_outputs.ps1` | 对产物签名（`package.sh` 会自动调用） |
| `scripts/_archive.py` | 产物归档 |
| `scripts/test_model_pack.py` | 模型包测试 |

---

## 四、版本号规范

格式：`{端标识}-{channel}-v{主}.{次}.{修订}-{YYYY}.{MMDD}.{HH}.{mm}`

```
windows-release-v1.0.0-2026.1028.09.35
windows-beta-v0.9.7-2026.1028.09.35
android-beta-v0.9.7-2026.1028.09.35
```

- **端标识放在渠道前面**（2026-10-28 变更）：之前两端版本串完全一样，
  混在同一批产物里无法分辨来源
- **端标识全小写**：`windows` / `android`
  - 由 `TRANSLATOR_PLATFORM` 注入（大小写不敏感）
  - 未注入时按 `CARGO_CFG_TARGET_OS` 推断（交叉编译 Android 自动得到 `android`）
  - 都没有 → 退回 `windows`
- **单一来源**：Rust `build.rs` 生成 → `package.sh` 写 `Translator/.version.ini`
  （`full` / `platform` / `channel` / `semver` / `num` 五段）→ `setup.iss`
  用 ISPP `ReadIni` 读取
- **PE 版本资源**用 `num` 段 = 四段纯数字 `0.9.7.1028`
  （带横杠或字母的串进不了 PE 资源；同日重复构建靠完整串区分）
- **递进规则**：
  - 正式版大迭代 → 更新全部数字：`v1.0.0` → `v2.0.0`
  - 测试版 → 只改后面数字，前面恒为 0：`0.9.1` → `0.9.8` → `0.9.12` → `0.9.45` → `0.10.56`
  - 上一级变动，下级归零（通用准则）
- **通道白名单**（Maven 语义 7 种）：`snapshot` / `alpha` / `beta` / `pre` /
  `release` / `stable` / `current`
- **4 位年**；**不加时间戳括号**；**关于页只显示版本号本身**
- ⚠️ **时间只取一次**（脚本开头）—— Rust 编译耗时会让分组阶段重取 `date` 导致版本对不上
- ⚠️ **渠道解析不能用正则按位置猜**（`about_page.dart` 的 `channelOf`）：
  端标识与渠道同为小写字母，`^([a-z]+)-` 会把 `windows` 当成渠道。
  改为**渠道白名单精确匹配**（新格式取第 2 段，旧格式取第 1 段，向后兼容）

---

## 五、目录结构

```
Translator/
├── translator_core/              # 源码
│   ├── core/                     # Rust 核心（采集/断句/识别/翻译/字幕/转写盘）
│   │   └── src/
│   │       ├── pipeline/engine.rs   # 主循环（feed_asr / commit_line）
│   │       ├── asr/sherpa.rs        # ASR 会话与热词
│   │       ├── audio/               # 采集 + AGC
│   │       └── ffi_session.rs       # FFI 入口（create/start/stop）
│   ├── ui/lib/                   # Flutter UI
│   │   └── services/lyrics_window.dart  # 悬浮字幕窗
│   ├── models/                   # 离线模型
│   └── scripts/                  # ★ 所有构建与验证脚本
├── src/setup.iss                 # Inno Setup 安装器脚本
├── outputs/                      # 当前产物（交付用）
├── last_good/                    # 上一次验证通过的产物（回滚用）
├── .staging/                     # 临时组装目录
├── dist/                         # 编译好的安装器（package.sh 自动生成）
└── .version.ini                  # 版本号四段（Rust 侧唯一来源）
```

---

## 六、硬性约定

### 构建安全
- ✅ `package.sh` **必须验证通过才覆盖** `last_good` / `outputs`；验证不过 `die`
- ✅ 破坏性验证（安装/卸载/清理）**只能由用户手动执行**
- ✅ 交付前用**只读**体检核对：产物哈希/时间戳、产物 mtime 晚于所有源文件

### 360 误报防护
- ✅ exe 必须带 PE 版本资源
- ❌ **禁止** VBS 隐藏启动
- ❌ **禁止**向 `$PLUGINSDIR` 释放裸 `.ps1`
- ❌ **禁止**安装阶段改用户 PATH / 在安装尾声手工写注册表键（改用 `[Registry]` 段）
- ✅ ORT + sherpa + Bergamot 全部静态链接（sherpa 用 static-MT 包）

### Flutter 插件 DLL
- ✅ **不要枚举插件**，`package.sh` 把 `Release/` 下所有 DLL 一并带走（除 ORT/sherpa 黑名单）
- ❌ 漏一个 `*_plugin.dll` → 安装后 exe 启动即报「找不到 xxx.dll」

### 托盘图标
- ✅ **必须用 `.ico` 文件路径**（`NOTIFYICONDATAW.hIcon` 是 HICON，由 `LoadImage` 加载）
- ❌ **禁止**传 base64 PNG data URI → API 全成功、日志打「已就绪」，但通知栏完全不显示
- ✅ `app_icon.ico` 必须进产物

### 安装器（Inno Setup 6.7.3）
- ✅ 注册表一律用根键 `HKA`（写死 `HKLM` → 用户模式报 `RegCreateKeyEx` 错误码 5）
- ✅ `PrivilegesRequired=admin` + `PrivilegesRequiredOverridesAllowed=dialog` +
  `DefaultDirName={autopf}\Translator`
- ✅ `[Code]` 里行首不能出现 `#`（ISPP 当成预处理指令 → 编译失败）
- ❌ 文件关联不要用 `HKCR`，用 `HKA` + `Subkey: "SOFTWARE\Classes\..."`
- ISCC.exe 位置：自行安装 Inno Setup 6 后，`package.sh` 会依次探测
  `tools/innosetup/`、`tools/innosetup/app/`、`Program Files (x86)/Inno Setup 6/`

---

## 七、排障

| 现象 | 可能原因 |
|---|---|
| cargo 报 `could not execute process build-script-build` / `拒绝访问 (os error 5)` | 环境块有大小写重复的代理变量（`HTTPS_PROXY` + `https_proxy`）→ `package.sh` 第 0 步已去重 |
| 手工 cargo build 触发 CMake 策略 panic | 缺 `BERGAMOT_PREBUILT` / `BERGAMOT_SRC_DIR` / `SHERPA_ONNX_LIB_DIR` |
| 启动即退出，无窗口 | `translator_core.dll` 未与 exe 同目录 / 缺失 |
| 启动报「找不到 xxx.dll」 | 漏收 `*_plugin.dll` |
| 自检显示「采集不可用」 | 无音频输出设备 / 设备被独占 / 系统策略限制 |
| 无字幕产出 | 本机没在播放声音 |
| 转写盘写入失败 | 桌面不可写 / 磁盘满 / 杀软锁文件（自动降级为仅内存并提示） |
| 安装后找不到程序 | 注册表写死 `HKLM`（用户模式）→ 改用 `HKA` |
| `git push` 报 `Failed to connect to github.com:443` | 网络通道变了（代理软件关掉/换端口）→ 用 `git_net.sh` 自动适配（见下） |

---

## 七之二、Git 网络通道自动适配

### 为什么需要

代理软件的端口会变，也会被随手关掉。把端口写死进 `.git/config`
的 `http.proxy` 后，代理一关 `git push` 立刻失败，而且**必须手工改
配置**才能恢复——曾经就在 `.git/config` 里写死过 `127.0.0.1:7892`，
代理软件一关，仓库就完全推不动了。

现在改成**每次执行前实测**当前哪条通道真正可用，用完即弃。

### 用法

```bash
# 自动适配（探测 → 执行）
bash Windows/scripts/git_net.sh push
bash Windows/scripts/git_net.sh pull --rebase
bash Windows/scripts/git_net.sh fetch --tags

# 只探测，不执行 git
bash Windows/scripts/git_net.sh probe

# 跳过探测，强制直连
bash Windows/scripts/git_net.sh -- push

# 跳过探测，指定代理
bash Windows/scripts/git_net.sh --proxy http://127.0.0.1:7890 push
```

### 探测顺序

按优先级**逐个实测**（不是看端口在不在——沙箱/工具注入的代理端口
常在监听却走不通 GitHub）：

1. 环境变量代理（`HTTPS_PROXY` / `HTTP_PROXY` 等）——用户显式设定
2. Windows 系统代理（**只读** `HKCU\...\Internet Settings`）
   ——`ProxyEnable=1` 时优先；即使为 0 也把 `ProxyServer` 里的端口
   作为低优先级候选（用户可能只开了代理软件、没开"系统代理"开关）
3. 常见本地代理端口：7890/7891/7897（Clash）、10808/10809（v2rayN）、
   1080/1087/8118（Shadowsocks）、2080（sing-box）、4780、8888
4. 直连（不使用任何代理）

判定标准是**真实跑一次 `git ls-remote`**——urllib 说"能通"不等于 git
能通（两者 TLS 栈与代理解析路径不同）。

### 安全边界

- **不写** `.git/config`——通道通过 `git -c http.proxy=...` 仅对本次
  命令生效
- **不碰**系统代理设置——注册表只读（`QueryValueEx`），不改
  `ProxyEnable` / `ProxyServer`
- **不写**任何凭据文件、不回显令牌

### Git 代理相关的三个坑（实测）

1. **`http.proxy` 与 `https.proxy` 必须同时设**。只设
   `-c http.proxy=` 空值时，git 仍会回落到环境变量里的
   `https_proxy` 并继续走它（表现为超时）。
2. **同名 `-c` 选项后者覆盖前者**。所以顺序必须是
   `-c http.proxy= -c https.proxy= -c http.proxy=<真实地址>`，
   反过来会让代理被空值覆盖 → 退化成直连。
3. **走代理时要保留环境变量里的 `http_proxy`**。Git for Windows 只有
   在环境变量存在时才对 https 目标使用 CONNECT 隧道；清空环境变量后
   即使显式给了 `-c http.proxy=`，隧道也会失败（报
   `option to transmit`）。所以选代理时保留环境变量、选直连时清空。

---

## 八、文档索引

仓库内的详细文档：

- `translator_core/docs/DESIGN.md` — 架构设计
- `translator_core/docs/WINDOWS_SETUP.md` — Windows 构建环境
- `translator_core/docs/ANDROID_SETUP.md` — Android 构建环境
- `translator_core/docs/GLOSSARY.md` — 词库格式说明
- `NOTICE` / `LICENSE` / `SECURITY.md` / `CONTRIBUTING.md` — 合规与贡献流程

---

## 九、许可与合规（不可跳过）

本项目采用 **FSL-1.1-ALv2**（Functional Source License 1.1，ALv2 未来许可）。
自每个版本发布之日起满 2 年，该版本**自动转为 Apache License 2.0**。

### 关键约束

| 约束 | 说明 |
|------|------|
| **Redistribution 条款** | 再分发任何副本/修改/衍生作品时，**必须附带条款副本或链接**，且不得移除已有版权声明 |
| **Competing Use** | 禁止把本软件（或功能相同/实质相似的变体）作为商业产品或服务提供给第三方 |
| **自动转 Apache-2.0** | 满 2 年自动转换，无需申请 |
| **CLA 必须先落地** | 变更许可需贡献者同意 → **接受第一个 PR 之前**必须启用 CLA |

### ⚠️ 静态链接带来的额外义务

ORT / sherpa-onnx / Bergamot 等**静态链接**进了 `translator_core.dll`，
静态链接意味着这些组件的许可条款**直接适用于本产品的分发行为**：

- ONNX Runtime — MIT
- sherpa-onnx — Apache-2.0
- Bergamot — MPL-2.0（文件级 copyleft，修改了源文件就须公开那些文件）
- PCRE2 — BSD-3-Clause

→ 所以 `LICENSE` / `NOTICE` / `THIRD_PARTY_LICENSES/` **必须进产物、必须进安装包**。

### 已接入的合规门禁

| 门禁 | 位置 | 作用 |
|------|------|------|
| 许可文件缺失 → `die` | `Windows/scripts/package.sh` | 缺 `LICENSE`/`NOTICE`/`THIRD_PARTY_LICENSES/` 直接中止发布 |
| `LicenseFile` 校验 | `Windows/src/setup.iss` | 安装器许可页缺 RTF → 编译失败 |
| RTF 自动生成 | `package.sh` | 从 `LICENSE` 生成，条款永远一致 |

⚠️ **改 `LICENSE` 文本后无需手动改 RTF** —— `package.sh` 每次构建都会重新生成。
但**改了 `NOTICE` 或 `THIRD_PARTY_LICENSES/` 内容**后，也要重跑一次
`package.sh`（或手动同步 `Windows/outputs/`）才生效。

### 商业授权

需要 Competing Use 场景请联系 **2575633220@qq.com**。

---
