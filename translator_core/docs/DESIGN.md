# Translator — 跨平台离线实时转写翻译系统

> 目标平台：Windows 10+/11 · macOS 13+ (Apple Silicon / Intel) · Android 10+ (API 29+) · **鸿蒙 [卓易通](https://droitong.com) 模拟层（Android APK）**
> 硬约束：**完全离线**。安装后运行期零网络请求、零遥测、零云端依赖。

---

## 1. 需求映射与技术选型总览

| # | 需求          | 选型                                                        | 关键理由                         |
| - | ----------- | --------------------------------------------------------- | ---------------------------- |
| 1 | 捕获系统内放声音    | WASAPI loopback / ScreenCaptureKit / AudioPlaybackCapture | 各自平台唯一受支持的内放路径，均产出 PCM       |
| 2 | 完全本地离线      | 全部推理走本地 C++ 引擎，无任何 HTTP 客户端代码                             | 架构上不可能联网                     |
| 3 | 实时语音转文字     | **sherpa-onnx** 流式 Zipformer (int8)                       | 真流式（非滑窗伪流式），单一 C API 覆盖三端    |
| 4 | 实时 en→zh 翻译 | **CTranslate2** int8 + Marian `opus-mt-en-zh`             | 单引擎三端可用，CPU int8 量化，移动端可接受延迟 |
| 5 | 自定义格式词库扩展   | 三层机制：ASR 热词偏置 → 翻译前占位符保护 → 翻译后强制替换                        | 见 §5，这是本系统的核心差异化设计           |

> **目标平台补充**：本产品同时覆盖 HarmonyOS 上的**卓易通**兼容层。卓易通基于华为 iSulad 容器引擎承载 Android APK（运行底层为 Android 16），因此**不需要为鸿蒙单独开发一套**，而是复用 Android 端产物并针对容器的权限模型做适配。详见 **§2.5 卓易通（HarmonyOS）适配**，这是 Android 端之外必须单独处理的一节。

### 为什么不用 whisper.cpp

| 维度     | whisper.cpp                 | sherpa-onnx（选定）                     |
| ------ | --------------------------- | ----------------------------------- |
| 流式性质   | 30 秒窗口 + 滑窗，partial 结果会大幅跳变 | 原生 transducer 流式解码，状态连续             |
| 首字延迟   | 需积累 1~3s 才有实质输出             | 256 ms 编码帧即可出字                      |
| 热词偏置   | 靠 `initial_prompt`，效果弱且不可控  | 原生 `hotwords_file` + 打分，作用于解码器 beam |
| 内置 VAD | 需自行集成                       | 同一库内置 Silero VAD                    |
| 语言覆盖   | 多语种                         | 多语种（含 SenseVoice 中英日韩）              |
| 许可证    | MIT                         | Apache-2.0（更宽松，含专利授权）               |

whisper.cpp 的优势在**离线批转写准确率**，而本项目是**流式字幕**场景，稳定性权重高于绝对准确率。若后续需要，可在同一架构下作为"最终段重打分"的可选后端接入（`AsrBackend` trait 已预留）。

---

## 2. 音频捕获方案（需求 1）

### 2.1–2.2 桌面端（Windows + macOS）— flexaudio

**实现方式修订**：初版设计为手写 COM（Windows）/ ScreenCaptureKit 绑定（macOS），
实测三条自研路线全部受阻，最终改用成熟 crate。

| 尝试 | 结果 |
|---|---|
| 手写 COM vtable | `IAudioClient` 有 14 个方法，索引错一位即崩。**不可交付** |
| `windows` crate 0.61 | `AUTOCONVERTPCM` 常量值本身有误（`2147483648`，应为 `1<<25`），且缺采样率转换标志。feature 集不完整 |
| `cpal` | **根本不支持 loopback** —— Windows 后端只暴露输入设备。死路 |

**选定方案**：`flexaudio` 0.6.0（MIT）
- Windows：WASAPI loopback · macOS：CoreAudio taps
- 统一 API，`chunk_ms` 直接指定块长（默认 20，与本项目 `CHUNK_MS` 一致）
- `exclude_self` 排除自身播放 → 避免转写自己的提示音
- `ProcessLoopback` + `ProcessMode::Exclude` → 按进程排除（卓易通容器内可用）
- **pull 模型**（`poll_chunk()`）而非 callback → 天然规避"音频回调里等下游"的风险（§6.2）

**版本要求修订**：flexaudio 的 CoreAudio taps 要求 **macOS 14.4+**，
比初版设计的 13.0 更严。13.0~14.3 无系统音频 API → 降级麦克风路径。

**关键事实**：
- Windows loopback **无需任何用户授权**（三端里体验最好）
- 部分 DRM 内容与明确 opt-out 的应用会输出静音，这是 OS 层保护，无法绕过
- macOS 需 TCC「屏幕录制」授权，**无法静默申请**，必须三段式引导：
  检测权限 → 打开系统设置面板 → 重新检测
- macOS 15+ 需 App 签名（含 ad-hoc）才能稳定获得 TCC 授权

### 2.3 Android — AudioPlaybackCapture + 麦克风降级

- **接口**：`MediaProjection` 授权 → `AudioPlaybackCaptureConfiguration` → `AudioRecord`。API 29+。
- **强制约束**：
  - 必须跑**前台服务**（Android 14+ 需 `foregroundServiceType="mediaPlayback"` + `FOREGROUND_SERVICE_MEDIA_PROJECTION` 权限）。
  - MediaProjection 授权**不可持久化**，App 每次冷启动都要重新弹窗确认。这是 OS 设计，绕不过。
- **回退路径**：当用户拒绝或应用不被捕获时，切换到麦克风 `AudioRecord`（`MediaRecorder.AudioSource.VOICE_COMMUNICATION`，系统自带 AEC 降噪），并用 `AcousticEchoCanceler` 主动消回声。
- **已知限制（必须提前告知用户）**：
  - 受 DRM 保护的内容（Netflix / 爱奇艺 / B站部分视频）**必然静音**，无法绕过。
  - 播放方可通过 `android:allowAudioPlaybackCapture="false"` 关闭被采集。
  - 同一 App 自身播放的声音默认捕获不到（`setAllowAudioPlaybackCapture(false)` 自排除）。

### 2.4 统一音频契约

三端捕获层全部收敛到同一个出口：

```rust
pub struct PcmChunk {
    pub samples: Vec<f32>,   // 16_000 Hz, mono, [-1.0, 1.0]
    pub capture_time_ms: u64, // 单调时钟，用于对齐字幕时间戳
    pub overrun_frames: u32,  // 丢弃的帧数，用于上报健康度
}
```

---

### 2.5 卓易通（HarmonyOS）适配

#### 2.5.1 平台性质判定

卓易通是华为上架的**跨生态应用兼容工具**，不是独立操作系统：

| 维度 | 事实 | 对本项目的影响 |
|---|---|---|
| 承载方式 | 华为自研 **iSulad 轻量级容器引擎**下的安卓沙盒 | APK 形态 → **复用 Android 全部产物**，无需 ArkTS 重写 |
| 运行底层 | 已升级至 **Android 16**（HarmonyOS 7.0 全量适配） | 可用 API level 高，`FOREGROUND_SERVICE_MEDIA_PLAYBACK` 等均可用 |
| 非虚拟机 | 官方明确"采用非虚拟机方案，减少性能损耗" | 性能损耗可控，但仍弱于原生 |
| 分发形态 | 卓易通应用市场下载安装，或文件互传导入 APK | 需提供独立安装说明（无法走 Google Play） |
| 权限管控 | 应用信息界面**逐项手动授权** | 权限引导必须适配"非系统弹窗"的交互 |

**结论**：本节的所有工作都是"**在 Android 实现之上加一层能力探测与降级**"，而非新增平台。代码上体现为 `AndroidRuntime` 枚举分支，而不是新的 `platform/` 模块。

#### 2.5.2 核心风险：内放捕获在容器内大概率不可用（已调研确认）

这是本节最需要谨慎对待的一点。调研结论：**按不可用来设计，不要赌它能用。**

`AudioPlaybackCapture` 依赖三重前提，卓易通容器环境下全部受阻：

| 前提 | 容器内状况 |
|---|---|
| **MediaProjection 授权** | 需宿主 HarmonyOS 把投屏授权**代理**进容器。用户只对"卓易通"授权，内部安卓应用并非授权主体 |
| **AudioPolicy 全局混音** | 容器 VFS 与进程树与宿主**物理隔断**，能否读到宿主 App 的播放流存疑 |
| **跨进程音频可见性** | 容器进程树与鸿蒙主系统进程树完全解耦 |

**新增确认的硬约束（2026-10-16 调研）**：容器安装时静态分析 `AndroidManifest.xml`，提取敏感权限（`READ_PHONE_STATE`、`SYSTEM_ALERT_WINDOW`、**`FOREGROUND_SERVICE_MEDIA_PROJECTION`**）与 `targetSdkVersion`，与白名单做特征码比对；**不达标则拒绝分配虚拟化资源，切断系统 API 调用**。→ 前台服务 + MediaProjection 这条路存在被直接拦截的实际风险。

**Native 库 ABI 校验**：容器加载进程时检查 `.so` 是否含非标准 ABI 调用或未授权反射，检测到即发 `SIGKILL`。→ Flutter 引擎与 sherpa-onnx 的 native 库必须确认为标准 `arm64-v8a` 且不含非常规反射，否则**启动即闪退**。
3. **跨进程音频可见性** —— 即使前两条通��，容器也不在宿主的音频客户端列表里。

**因此采集策略设计为运行时能力探测，而非编译期假定**：

```
启动 → 探测 AudioPlaybackCapture 可用性
        │
        ├─ 可用 → 正常内放路径
        │
        └─ 不可用 → 自动降级
                     │
                     ├─ ① 麦克风 + 主动回声消除（默认降级目标）
                     │    AudioRecord + VOICE_COMMUNICATION
                     │    + AcousticEchoCanceler / NoiseSuppressor
                     │    + 提示"当前为麦克风模式，外放声音需佩戴耳机"
                     │
                     └─ ② 用户手动方案（最高质量，可选）
                          在卓易通内安装"虚拟声卡"类应用，
                          把宿主系统音频引入容器，再由容器内麦克风通道采集
```

**这一点必须在 UI 中明示，不能静默降级**。用户在卓易通里装好应用、点了开始，却发现只能录自己的声音而不知原因，是最差体验。因此启动页需要做一次 3 秒自检：显示"内放采集：可用 / 不可用（将使用麦克风）"。

#### 2.5.3 模型与性能适配

容器内 CPU 与内存均非原生水平，且有额外开销。适配措施：

| 项 | 原生 Android | 卓易通 |
|---|---|---|
| ASR 模型 | zipformer-en int8（72 MB） | 同；**避免加载 large 档**（200 MB，容器内加载慢且易触发低内存回收） |
| MT 模型 | opus-mt-en-zh int8（78 MB） | 同；beam 固定为 1 |
| 线程数 | `cores/2` | `max(1, cores/3)`，降低容器内调度争抢 |
| VAD hangover | 300 ms（稳定）/ 150 ms（速度） | 固定 **300 ms**，容器内存波动大，宁慢勿错句 |
| 模型存放 | 应用私有目录 | 同；**禁止依赖外部存储路径**，卓易通与宿主存储严格隔离（须走"文件互传"） |
| 大文件导出 | 直接写公共目录 | 必须先写容器私有目录，再经"文件互传"导出 |

**存储隔离是卓易通特有的硬约束**：容器无法直接读写手机存储内容。`context.getExternalFilesDir()` 在容器内指向容器空间。模型导入、字幕导出、词库文件全部必须走应用私有目录 + 卓易通的文件互传，这是与原生 Android 最大的流程差异。

#### 2.5.4 前台服务与保活

- 卓易通容器内的后台保活策略与原生 Android 不同，且容器可能被系统回收。
- 采集服务必须以 `foregroundServiceType="mediaPlayback"` + 常驻通知运行，通知文案明确提示"正在转写系统音频"。
- 增加**容器存活自检**：录音回调中断超过 2 秒即上报异常并尝试重启服务（原生 Android 不需要这层，卓易通需要）。
- 建议用户将卓易通加入电池优化白名单 —— 需在 UI 的"常见问题"里给出图文步骤。

#### 2.5.5 交付形态差异

| 项 | 原生 Android | 卓易通 |
|---|---|---|
| 分发 | Google Play / APK | 卓易通应用市场，或 APK 经华为分享 / 文件互传导入 |
| 安装包 | arm64-v8a 通用 | 同（卓易通已是 arm64 容器，无需 x86 变体） |
| 版本检查 | 走 Play | 应用内提示"可从卓易通市场更新"，不联网自动更新（离线约束） |
| 文档 | 应用内帮助 | **需额外提供《卓易通安装与授权指引》**，含截图的分步指引 |

---

#### 2.5.2b 其他已确认的容器约束（2026-10-16 调研）

| 约束 | 事实 | 对本项目的影响 |
|---|---|---|
| **存储隔离** | 容器内应用无法直接读鸿蒙相册/文档根目录，只能经"文件互传" | **352 MB 模型包的导入流程需专门设计**（见 §2.5.3） |
| **黑白名单** | 已上架鸿蒙原生 HAP 的应用禁止在卓易通内装同款 APK | 本产品无鸿蒙原生版，**不冲突** |
| **容器休眠** | 默认休眠，不打开不加载安卓运行环境；用完进程直接销毁 | 采集需前台服务常驻 + 断流自检（见 §2.5.4） |
| **个性化不跟随** | 系统字体、深色模式不同步进容器 | Flutter 自带主题，不受影响 |
| **性能折损** | 功耗、发热、帧率均有折损 | 模型档位与线程数需下调（见 §2.5.3） |

## 3. 离线识别与翻译模型部署（需求 2/3/4）

### 3.1 模型清单（全部可离线导入，运行期不下载）

| 角色           | 模型                                                    | 量化   | 体积      | 许可证                |
| ------------ | ----------------------------------------------------- | ---- | ------- | ------------------ |
| ASR（英文）      | `sherpa-onnx-streaming-zipformer-en-blenk-256ms-int8` | int8 | ~72 MB  | Apache-2.0         |
| ASR（可选·高精度）  | `sherpa-onnx-streaming-zipformer-large-en-256ms-int8` | int8 | ~200 MB | Apache-2.0         |
| ASR（可选·中英混说） | `SenseVoice-Small`                                    | int8 | ~230 MB | SenseVoice License |
| VAD          | `silero_vad.onnx`                                     | fp32 | ~2 MB   | MIT                |
| MT en→zh     | Helsinki-NLP `opus-mt-en-zh`                          | int8 | ~78 MB  | CC-BY-4.0          |
| MT（可选·高质量）   | `opus-mt-en-zh` base                                  | int8 | ~300 MB | CC-BY-4.0          |

**架构性优势**：ONNX 权重**与 CPU 架构无关**，同一份 int8 权重在 x86_64 / arm64 / macOS arm64 上通用。只有 sherpa-onnx 的编译产物（`.dll`/`.dylib`/`.so`）需要分架构打包。

### 3.2 模型仓库（Model Store）

```
<AppData>/DSHLiveTranslator/models/
├── registry.json          # 元数据 + sha256 + 许可证 + 体积
├── asr/
│   ├── zipformer-en-256ms-int8/
│   │   ├── encoder-epoch3-99.int8.onnx
│   │   ├── decoder-epoch3-99.int8.onnx
│   │   ├── joiner-epoch3-99.int8.onnx
│   │   └── tokens.txt
│   └── sensevoice-small/
├── vad/silero_vad.onnx
└── mt/
    └── opus-mt-en-zh-int8/
        ├── model.bin          # CT2 格式
        └── config.json
```

**导入机制**（满足"完全离线"）：

1. 用户在设置页选择模型包文件（`.dshm` = zip，含 `manifest.json` + 权重）。
2. 校验每个文件的 **sha256** 与大小，与 `manifest.json` 声明比对。
3. 校验通过 → 解压到临时目录 `models/.staging-<uuid>/`。
4. **原子提交**：全部就绪后 rename 到正式目录（`models/asr/<id>/`），中途失败只删临时目录。
5. `registry.json` 用「写临时文件 → rename」更新，避免进程崩溃导致仓库损坏。
6. 磁盘空间不足 ≥2 倍包体积时提前拒绝并提示。

安装包内**只内置最小可运行集**（zipformer int8 + VAD + MT int8，约 152 MB），其余作为可选模型包由用户手动导入。这样安装包体积可控，同时满足开箱即用与零联网。

### 3.3 运行期约束

- 进程启动时**不主动建立任何网络连接**。代码层面移除所有 HTTP 客户端依赖。
- 模型缺失时不崩溃，降级为引导 UI（"请导入 X 模型"），禁用对应能力。
- 首次运行提供"离线自检"：连续跑 200 帧音频，输出各阶段耗时与 RTF，作为性能基线。

---

## 4. 翻译引擎选型

### 4.1 为什么最终选 Bergamot 而非 CTranslate2

两者同为 Marian NMT 框架、架构同源，但**构建方式**是决定性因素：

| | CTranslate2（Rust 绑定 2.1.1） | **Bergamot（选定）** |
|---|---|---|
| 构建方式 | `ctranslate2-sys` **联网下载**预编译包 | 纯 CMake 源码编译 |
| 离线构建 | ❌ 实测代理 502 即失败 | ✅ 仅需 C++17 编译器 |
| 输入 | **token 级**（`&[Vec<String>]`），需自接 SentencePiece | **原始文本**，内置 SPM |
| 产物 | 动态库 | `libbergamot-translator.a` 静态库 |
| 许可 | MIT | Mozilla MPL-2.0 |
| 背书 | Intel | Mozilla（Firefox Translations 同款） |

**决定性因素是离线性**。CTranslate2 的 build-dependency 引入了 `ureq` / `rustls` / `http` 用于下载预编译包，与「构建与运行双离线」的硬约束直接冲突。该问题由 `scripts/check_offline.py` 门禁发现并拦截。

**附带收益**：Bergamot 收原始文本、内部自带 SentencePiece，省掉一层分词器适配与对应的 bug 面。

### 4.2 权重格式差异（重要修正）

⚠️ **Bergamot 权重与 Marian PyTorch 权重不同源，不能直接互换。**

| | Bergamot | Marian (`opus-mt-en-zh`) |
|---|---|---|
| 权重 | `model.bin` + `registry.json` | `pytorch_model.bin` |
| 分词器 | 内置于权重目录 | `source.spm` / `target.spm` |
| 获取 | Bergamot 模型仓库（Mozilla 分发） | HuggingFace |

这修正了本文早期版本"两者共享同一份模型目录结构"的错误表述。
实际影响：**不能**用 Marian 权重直接喂给 Bergamot，需获取 Bergamot 格式的
en→zh 权重。这会影响模型打包流程（§3.2）。

降级链：Bergamot 不可用 → 暂不提供替代引擎。Marian 权重若要使用需经
`ct2-transformers-converter` 转 CT2 格式，而那又回到联网构建问题，故本期不保留该路径。

### 4.3 线程与质量档位

- Bergamot 的 `translate` 接口无内部可变状态（模型只读），
  故 `BergamotTranslator` 声明为 `Send + Sync`，可用 `Arc` 共享
- 质量档位差异体现在**权重本身**（tiny / base），而非代码参数。
  两档共用同一引擎代码，切换仅需换权重目录
- `translate_multi` 批量接口可摊薄固定开销，用于字幕追赶场景

### 4.4 跨 FFI 安全

Bergamot 是 C++ 库，Rust 侧必须遵守：

- **跨 FFI 不得 panic** —— 全部调用包在 `catch_unwind` 内，转错误码
- `panic = "unwind"`（不可用 `abort`，否则 FFI 边界崩溃会直接杀宿主进程）
- Bergamot 返回的 C 字符串**必须**用 `bergamot_translator_free_string` 释放，
  否则跨 FFI 内存泄漏
- 裸指针 `Send/Sync` 需显式 `unsafe impl`，并在注释中写明安全依据

### 4.5 构建集成

`core/build.rs` 在启用 `mt-bergamot` feature 时调用 CMake 编译 Bergamot 静态库：

```bash
# 一次性获取源码（构建机）
git submodule update --init --recursive

# 构建时的环境变量
BERGAMOT_SRC_DIR    # 直接指定源码目录
BERGAMOT_PREBUILT   # 指向已编译的静态库，跳过 CMake
MEFEASON_BUILD_NATIVE=0  # 跳过原生构建（交叉编译 host 侧）
```

源码缺失时 `build.rs` 只发 `cargo:warning` 而非 panic —— 核心逻辑与测试
仍可编译运行。

---

## 5. 自定义词库管理机制（需求 5，核心设计）

用户导入的自定义格式文件需要同时作用于**三个环节**，因为术语问题会在链条上任意一环出错。

### 5.1 支持的文件格式

| 格式 | 扩展名 | 说明 |
|---|---|---|
| TSV | `.tsv` | 制表符分隔，兼容性最好，首选 |
| CSV | `.csv` | 支持引号转义与字段内逗号 |
| JSON | `.json` | 完整字段，程序化生成友好 |
| YAML | `.yaml/.yml` | 便于手写，含分组与注释 |
| 简单键值 | `.txt` | `source<TAB>target` 每行一条 |

格式细节、字段定义与合并规则见 `docs/GLOSSARY.md`。

### 5.2 三层作用机制

```
       英文 ASR 文本
            │
    ┌───────▼────────┐  Layer A
    │ ASR 热词偏置   │  词库术语喂给解码器 → 术语被听对
    └───────┬────────┘
            │  带热词的转写文本
    ┌───────▼────────┐  Layer B
    │ 占位符保护     │  Aho-Corasick 最长匹配 → ▁X{n}X
    └───────┬────────┘
            │  含哨兵的安全文本
    ┌───────▼────────┐
    │ Bergamot en→zh │  Marian 不翻译未登录片段，哨兵原样透传
    └───────┬────────┘
            │  含哨兵的译文
    ┌───────▼────────┐  Layer C
    │ 哨兵还原 +     │  ▁X{n}X → 词库指定译法
    │ 后置强制替换   │  + 正则兜底修正
    └───────┬────────┘
            │
       最终中文译文
```

**Layer B 是本方案的关键。** 直接翻译 `We deployed Kubernetes` 会得到
`我们昨天部署了库伯内特斯`，无论译文多准确。哨兵选 `▁X{n}X`：
`▁`(U+2581) 是 SentencePiece 的显式词边界标记，保证这串字符被切成
独立且未登录的片段，模型不会尝试翻译它。

**技术要点**：
- 必须用 `MatchKind::LeftmostLongest` —— `Standard` 在同起点只报一个
  （实测取最短），`JavaScript` 会被切成 `Java` + `Script`
- `Entry::match_key()` **不折叠空白**（折叠会改字节数，破坏坐标映射导致
  切片错位）；去重另用 `storage_key()`
- `SnapshotGuard` 是必需正确性保障：哨兵索引 `n` 是快照内局部索引，
  处理中途换快照会把 A 词还原成 B 词

### 5.3 词库运行时管理

- **热重载**：`notify` 监听 + 250 ms 防抖，构建新快照后原子替换。
  构建失败保留旧快照（语法错误不得清空正在工作的词库）
- **词库变更不影响 ASR**：热词通过 `create_stream_with_hotwords` 绑定到
  session，新建会话即生效，**不重建 recognizer**（省 2~3 秒）
- **多文件合并**：同 `source` 时 `enabled: true` 胜出；多条 true 则后加载的胜出
- **命中统计**：`asr_hits` / `mt_hits` / `regex_hits` 分开计数，UI 提供
  "零命中词条"筛选帮用户清理失效词条
- **自增长词库**：导出 `mt_hits >= 1` 的词条整理成新词库

---

## 6. 流水线、线程模型与延迟预算

### 6.1 线程拓扑

```
┌──────────────┐   SPSC 无锁    ┌───────────┐   消息   ┌────────────┐
│ 捕获线程      │──RingBuffer──▶│ VAD+分段器 │─────────▶│ ASR Session │
│ (平台原生)    │              │ 512s/32ms │          │ (独占 stream)│
└──────────────┘              └───────────┘          └─────┬──────┘
     │ 无锁写，永远不阻塞                                  │ Partial/Final
     │ 背压策略：宁可丢帧，也不阻塞捕获                        ▼
     │                                                  ┌────────────┐
     │                                                  │ 词库保护    │
     │                                                  │ Layer A/B  │
     │                                                  └─────┬──────┘
     │                                                        │ 分片提交
     ▼                                                        ▼
┌──────────────┐                                         ┌────────────┐
│ 健康度/丢帧   │                                         │ MT (Bergamot)│
│ 统计上报      │                                         │ Arc 共享    │
└──────────────┘                                         └─────┬──────┘
                                                                  │
                                                                  ▼
                                                        ┌──────────────┐
                                                        │ 字幕聚合器    │
                                                        │ partial→final│
                                                        └──────┬───────┘
                                                               ▼
                                                        UI / 导出
```

`sherpa-onnx` 的 `OnlineRecognizer` 与 `OnlineStream` 均为 `Send + Sync`，
真正的约束是**同一 stream 不可并发调用**，因此 session 独占一条 stream 即可。

### 6.2 关键实现约束

- **捕获线程绝不能被下游阻塞**。Ring buffer 满时丢最旧帧并计数上报，
  宁可丢音频也不能让音频回调卡顿（Windows 上卡顿会导致系统判定音频故障）。
- **partial 结果不直接上屏**。流式识别器在句子中途会大幅改写前文，
  直接显示会剧烈跳动。partial 只用于"预览态"，**只有 VAD 判定断句后的
  final 段才进入正式字幕行**。这是稳定性最关键的一条。
- **断句由 VAD 独占**。sherpa-onnx 的 `enable_endpoint` 必须关闭，
  两套断句逻辑并存会导致句子被切碎。
- **MT 背压**：翻译慢于 ASR 时，final 段先以"待翻译"状态入队，
  译文到达后再补。UI 显示 `EN ▸ ░░░` 表示翻译中，**绝不阻塞转写显示**。

### 6.3 延迟预算（target）

| 阶段 | 桌面（Win/Mac） | Android 中端机 |
|---|---|---|
| 采集帧 | 10 ms | 10 ms |
| VAD + 断句（含 300 ms hangover） | 300 ms | 300 ms |
| ASR 流式出字（首） | ~500 ms | ~1100 ms |
| ASR final（断句后） | ~400 ms | ~900 ms |
| 翻译（10 token 句） | ~250 ms | ~800 ms |
| **字幕 EN 可见延迟** | **~0.9 s** | **~1.8 s** |
| **字幕 ZH 可见延迟** | **~1.3 s** | **~2.6 s** |

---

## 7. 跨平台一致性实现思路

一致性不是靠"三端写三遍"，而是靠**架构强制**：所有平台差异被压缩到 4 个
trait 后面，核心逻辑只有一份实现。

### 7.1 平台抽象边界

```rust
pub trait SystemAudioCapture { /* §2 */ }
pub trait PermissionGate     { fn status(&self) -> PermState; fn request(&self) -> PermState; fn open_settings(&self); }
pub trait OverlayWindow     { fn show(&self, view: &SubtitleView); fn set_always_on_top(&mut self, bool); }
pub trait ConfigStore       { fn load(&self) -> AppConfig; fn save(&self, &AppConfig) -> Result<()>; }
```

**AndroidRuntime 枚举**（卓易通适配的代码落点）：

```rust
pub enum AndroidRuntime { Native, ZhuoYiTong }   // 见 config.rs
pub enum CaptureCapability { SystemAudio, MicrophoneOnly { reason }, Unavailable { reason } }
```

平台特定代码只允许出现在实现这 4 个 trait 的模块，以及 `platform/android/runtime.rs`。
DSP、ASR、MT、词库、字幕聚合**全部在共享 crate 内，不含任何 `#[cfg(target_os)]`**。

### 7.2 强制一致性的机制

1. **FFI 契约唯一**：`translator-main` 暴露纯 C ABI，Flutter 侧由
   `flutter_rust_bridge` 生成绑定。禁止跨语言传 Rust 复杂类型。
2. **参数固定化**：帧长 512 样本（32 ms @16 kHz）、VAD 阈值、hangover、
   beam 全部走 `config.rs` 同一份默认值；三端仅允许通过设置页覆写。
3. **黄金测试**：仓库内固定一段 60 s 双语音频 fixture，三端 CI 各跑一遍，
   断言输出的**归一化**转写 + 译文完全一致（归一化 = 小写折叠 + 去标点 +
   折叠空白）。任一端输出变化即 CI 失败。
4. **统一时间基准**：所有时间戳用单调时钟毫秒，在 UI 层才转系统时间。
5. **统一导出格式**：`.srt` / `.vtt` / `.txt` 三端字节级一致。

### 7.3 UI 层选型

| 方案 | 优点 | 缺点 | 结论 |
|---|---|---|---|
| **Flutter + `flutter_rust_bridge`** | 一套 UI 覆盖三端；桌面端成熟 | Android 需写 Kotlin 插件接 MediaProjection（不可避免）；构建链较长 | **选定** |
| 三套原生 UI | 平台观感最佳、无桥接开销 | 3 倍 UI 工作量与测试面 | v2 备选 |
| Tauri v2 | 桌面体验好 | 移动端支持不成熟，权限模型不匹配 | 否决 |

---

## 8. 代码结构

```
TranslatorCore/
├── AGENTS.md                    # 规则文件（跨会话最高杠杆上下文）
├── .gitmodules                  # Bergamot 源码 submodule
├── core/                        # Rust 核心 crate
│   ├── Cargo.toml
│   ├── build.rs                 # CMake 集成 Bergamot 静态库
│   └── src/
│       ├── lib.rs
│       ├── config.rs            # ★ 唯一参数真相源 + AndroidRuntime
│       ├── error.rs
│       ├── offline.rs           # ★ 离线性契约（可执行证据）
│       ├── audio/
│       │   ├── mod.rs           # PcmChunk 契约 + VAD 帧切分
│       │   ├── ring.rs          # SPSC 无锁环形缓冲
│       │   └── resample.rs      # → 16k mono f32
│       ├── vad/
│       │   └── segmenter.rs     # ★ 断句独占（hangover + 强制切分）
│       ├── asr/
│       │   ├── mod.rs           # AsrBackend/AsrSession trait + ChunkSplitter
│       │   ├── mock.rs          # 零依赖参考实现（测试与黄金基准）
│       │   └── sherpa.rs        # sherpa-onnx 后端
│       ├── mt/
│       │   ├── mod.rs           # MtBackend trait + 长文本切分
│       │   ├── mock.rs          # 零依赖参考实现
│       │   └── bergamot.rs      # Bergamot 后端（手写 FFI）
│       ├── glossary/            # ★ 需求 5 核心
│       │   ├── mod.rs
│       │   ├── schema.rs        # Entry + match_key/storage_key
│       │   ├── sentinel.rs      # Layer B 哨兵保护与还原
│       │   ├── snapshot.rs      # 快照 + ArcSwap + SnapshotGuard + 统计
│       │   └── parse/           # tsv / csv / json / yaml / plain
│       ├── subtitle/
│       │   └── mod.rs           # 聚合 + SRT/VTT/TXT 导出
│       ├── platform/            # ★ 唯一允许 cfg(target_os) 的地方
│       │   ├── mod.rs           # 4 个 trait + AndroidRuntime 分支
│       │   ├── windows/         # WASAPI loopback + 热插拔
│       │   ├── macos/           # ScreenCaptureKit + TCC
│       │   └── android/
│       │       ├── runtime.rs   # ★ 卓易通探测与能力降级
│       │       └── storage.rs   # ★ 容器存储隔离
│       └── modelstore/          # 模型仓库 + .dshm 导入
├── tests/
│   └── pipeline.rs              # ★ 集成测试（3 条核心不变量 + MT 契约）
├── scripts/
│   └── check_offline.py         # ★ 离线性构建门禁
├── ui/                          # Flutter 应用（待接入）
├── docs/
│   ├── DESIGN.md
│   ├── GLOSSARY.md
│   └── ZHUOYITONG.md            # 卓易通安装与授权指引（待写）
└── third_party/
    └── README.md                # Bergamot 源码说明
```

---

## 9. 风险登记与缓解

| 风险 | 等级 | 影响 | 缓解 |
|---|---|---|---|
| **卓易通 native 库 ABI 校验闪退** | **最高（可能作废方案）** | 容器内启动即 SIGKILL | 真机优先验证；若不通过需改纯 ArkTS 重写（§12 第 4 条） |
| **卓易通 `FOREGROUND_SERVICE_MEDIA_PROJECTION` 被白名单拦截** | 高 | 内放采集根本起不来 | 运行时探测 + 自动降级（§2.5.2） |
| **卓易通容器内 `AudioPlaybackCapture` 不可用** | 高（很可能必然） | 内放采集失效，只剩麦克风 | 启动期 3 秒自检 + 自动降级 + **UI 明示**；提供虚拟声卡方案 |
| **卓易通存储与宿主隔离** | 高（已确认） | 模型/词库/字幕无法直接读写 | 全部走应用私有目录 + "文件互传"，`storage.rs` 统一封装 |
| **卓易通容器休眠 / 进程销毁** | 中高（已确认） | 采集中途断流 | 前台服务常驻 + 2 秒断流自检重启 + 引导加电池白名单 |
| **卓易通内性能折损** | 中（已确认） | 延迟升高、发热 | 容器内线程降到 `cores/3`、禁 large 档 |
| **Bergamot 权重与 Marian 不同源** | 中 | 需另找 Bergamot 格式权重 | 见 §4.2，已在模型打包流程中体现 |
| Android DRM / opt-out 应用无法采集 | 高（不可绕过） | 部分视频源静音 | 麦克风回退路径；UI 明示"受保护内容不受支持" |
| macOS 未签名构建 TCC 反复掉权限 | 高 | macOS 端不可用 | Release 强制签名，debug 需知会用户 |
| MediaProjection 每次冷启动需重新授权 | 中（OS 设计） | Android 体验断裂 | 前台服务 + 引导页说明 |
| 哨兵被模型改写导致还原失败 | 中 | 术语译文错乱 | 还原前后计数校验，失败触发 Layer C + UI 标记待复核 |
| 第三方许可（MPL-2.0 / Apache-2.0）未随包 | 中 | 分发合规风险 | `registry.json` 记录每个模型许可；附 `THIRD_PARTY_LICENSES` |

---

## 10. 交付阶段

| 阶段 | 内容 | 状态 |
|---|---|---|
| M1 | 核心骨架 + 音频层 + VAD + 分段器 | ✅ 完成 |
| M2 | sherpa-onnx ASR + 热词偏置 | ✅ 完成（编译通过，待真实模型验证） |
| M3 | 翻译后端 + 字幕聚合 | ✅ Bergamot 后端（待编译 Bergamot 源码 + 权重） |
| M4 | 词库三层机制 + 5 种格式 + 热重载 | ✅ 完成 |
| M5 | Flutter UI + 三端采集插件 | ⬜ 未开始 |
| M6 | 模型仓库 + `.dshm` 导入 + 黄金测试 | ⬜ 未开始 |

---

## 11. 已确认决策（2026-10-16）

| # | 决策点 | 结论 |
|---|---|---|
| 1 | UI 框架 | **Flutter + `flutter_rust_bridge`** |
| 2 | 安装包模型 | **最小集 + 高精度 ASR**（≈352 MB） |
| 3 | 翻译模型 | **双档策略**：默认快档保流畅，精准档可选导入 |
| 4 | 说话人分离 | **暂不做**（`AsrBackend` 已留扩展位） |
| 5 | 项目命名 | **Translator** |
| 6 | 鸿蒙卓易通兼容 | **纳入目标平台**，复用 Android 产物 + 运行时能力探测（§2.5） |
| 7 | 翻译引擎 | **Bergamot**（因 CTranslate2 构建期联网，违反离线性约束） |
| 8 | 离线性级别 | **构建期 + 运行期双离线**，`scripts/check_offline.py` 为门禁 |

---

## 12. 待验证事项（需真机实测）

以下判断基于公开技术资料，**必须在真机上验证后才能定稿**：

### 已通过公开资料确认（见 §2.5.2b）

- 容器存储与宿主隔离，必须走文件互传
- 黑白名单针对"已有 HAP 原生版"的应用，本产品不冲突
- 容器默认休眠 + 用完即销毁
- 容器会做 Manifest 权限白名单审计与 Native 库 ABI 校验

### 仍需真机实测

1. 卓易通容器内 `MediaProjection` 是否可用？能否拿到有效 token？
2. 容器内 `AudioPlaybackCapture` 能否读到宿主播放流？
3. `FOREGROUND_SERVICE_MEDIA_PROJECTION` 是否被容器权限白名单拦截？
4. **Flutter + sherpa-onnx 的 native 库能否通过 ABI 校验**？启动是否闪退？
   —— **风险最高，闪退即整个卓易通方案作废**
5. 352 MB 模型包经"文件互传"导入是否可行？容器私有目录容量上限？
6. 容器内 sherpa-onnx + Bergamot 的实测 RTF 与内存占用？
7. 容器内前台服务的实际保活表现与被回收时机？
8. 各机型上卓易通的 APK 安装成功率？
9. macOS 15+ 下 ad-hoc 签名的 TCC 授权稳定性？

---

## 13. 当前状态

- 测试：**150 单测 + 13 集成测试全绿**（default 与 `--all-features` 双配置）
- release 构建零警告；核心在原生库缺失时仍可编译（feature 隔离）
- 离线性门禁通过：运行期 52 包、构建期 1 包，均无网络类依赖
- 已实现：config / offline / audio / vad / asr(mock+sherpa) / mt(mock+bergamot) /
  glossary / subtitle / platform(traits)
- 未实现：Flutter UI、三端采集实现、模型仓库、黄金测试 fixture
