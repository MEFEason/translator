# Translator — 离线实时转写翻译（Windows）

完全离线的实时语音转写 + 翻译字幕工具。所有模型与推理均在本机完成，不发起任何网络请求。

## 已接入的真实后端

| 模块 | 实现 | 说明 |
|---|---|---|
| ASR 语音识别 | sherpa-ONNX 流式识别（zipformer-en-20M） | 离线英文语音识别 |
| MT 机器翻译 | Bergamot 离线神经翻译（en→zh） | 离线英译中 |

两项能力所需的全部推理代码（含 ONNX Runtime、sherpa、Bergamot）**全部静态链接进 `translator_core.dll`**。因此安装目录与磁盘上**不存在任何 `onnxruntime.dll` / `sherpa-onnx-*.dll`**，从根本上规避了杀软对「新增 / 修改系统 DLL」的误报。

## 运行要求

- Windows 10 及以上，64 位（x64）
- 无需安装 —— 解压后双击 `translator.exe` 即可
- 无需管理员权限
- 完全离线运行（不发起任何网络请求）

## 目录内容

| 文件 | 作用 |
|---|---|
| `translator.exe` | 主程序 |
| `translator_core.dll` | Rust 核心（采集 / 断句 / 识别 / 翻译 / 字幕 / 转写盘），静态链接 ORT + sherpa + Bergamot |
| `flutter_windows.dll` | Flutter 引擎 |
| `data/` | Flutter 资源（**不要删**） |
| `models/` | 离线模型（ASR + MT，约 94 MB） |
| `certs/` | 代码签名根证书（仅安装版写入系统根存储） |

`translator_core.dll` 必须与 `translator.exe` 同目录，Windows 加载器才能找到它。

## 性能档位（按真实资源占用推荐）

内置三档，由 `core/src/background.rs` 基于**实时 CPU 占用率、内存占用率、以及本机其他软件的运转情况**综合判定并推荐，而非简单软件黑白名单：

| 档位 | ASR 线程 | MT 线程 | 触发建议 |
|---|---|---|---|
| 轻量 | 1 | 1 | CPU ≥ 50% 或内存 ≥ 80% |
| 均衡（默认） | 2 | 2 | CPU ≥ 20% 或内存 ≥ 50% |
| 全力 | 4 | 4 | 系统空闲时 |

设置页会显示实测的 CPU / 内存占用与推荐档位，也可手动切换。后台有视频会议、音乐等软件运行时，工具会自动收敛线程，避免抢占系统资源。

## 使用

1. 双击 `translator.exe`
2. 启动自检完成（设置页可见 5 步结果：库 / ASR / 模型 / 词库 / 采集）
3. 字幕页点「开始转写」
4. 播放音频（视频、音乐、别人说话）→ 实时字幕（EN + ZH）

### 声源与说话人

默认同时开启两路，可分别开关：系统内放（对方）/ 麦克风（我）。说话人名字可自定义，以彩色徽章显示；两路各自独立断句。

### 转写盘

字幕实时写入桌面 `translate results\transcript-YYYYMMDD-HHMMSS.dsht`（append-only，带 CRC-32，断电最多丢最后一条）。

### 导出

字幕页导出按钮 → 保存为 `.txt` 对话记录（给人读，含说话人、EN/ZH 对照）。

## 安装（setup.iss）

- 安装路径：`C:\Program Files\Translator\release\<版本号>\`（多版本可共存）
- 静默写入代码签名根证书到 **HKLM 根存储**（通过注册表原生段写 Blob，**无安全警告弹窗**）
- 注册：卸载项、App Paths、`.srt` 文件关联、开始菜单 / 桌面快捷方式
- 卸载：经控制面板或卸载快捷方式，清理目录 / 注册 / 证书 / 关联，零残留

## 自己验证

```bash
# 真实后端跑通整条链路（需已构建 sherpa / bergamot）
bash translator_core/scripts/selftest.sh --asr-sherpa --mt-bergamot --seconds 12 --bench

# 模拟 UI 的完整 FFI 调用路径（含多声源、转写盘、历史回溯）
python translator_core/scripts/verify_ffi.py Translator 5

# 离线性门禁（三层）
python translator_core/scripts/check_offline.py
```

## 已知限制

| 项 | 状态 |
|---|---|
| 全透明置顶悬浮窗 | 未实现（当前为应用窗口内字幕） |
| Android / 卓易通 | 未接入 |
| 模型格式 | 当前为 Bergamot / sherpa 原生格式；`.dshm` 压缩包待后续 |

## 排障

| 现象 | 可能原因 |
|---|---|
| 启动即退出，无窗口 | `translator_core.dll` 未与 exe 同目录 / 缺失 |
| 自检显示「采集不可用」 | 无音频输出设备 / 设备被独占 / 系统策略限制 |
| 无字幕产出 | 本机没有在播放声音 |
| 转写盘写入失败 | 桌面不可写 / 磁盘满 / 杀软锁文件（会自动降级为仅内存并提示） |

## 构建

```bash
# 完整构建 + 发布（唯一入口：Rust / Flutter 构建、验证门禁、安装器编译、签名）
TRANSLATOR_SEMVER=0.9.27 TRANSLATOR_CHANNEL=beta   bash Windows/scripts/package.sh
TRANSLATOR_SEMVER=1.0.0  TRANSLATOR_CHANNEL=release bash Windows/scripts/package.sh

# 仅重新打包（复用已构建产物）
SKIP_BUILD=1 bash Windows/scripts/package.sh
```

> `package.sh` 会**自动编译安装器**（探测 `tools/innosetup/ISCC.exe` → 编译
> `src/setup.iss` → 签名 → 落到 `Translator/dist/`），不需要手动跑 ISCC。

完整脚本清单与硬性约定见项目根目录 `README.md`。
