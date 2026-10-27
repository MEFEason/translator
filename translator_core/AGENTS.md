# Translator — 项目规则

> 本文件是跨会话持久生效的最高杠杆上下文。**未写下来的项目知识 = 不存在。**
> 只放"读错就会写错代码"的信息；不重复 docs/ 里的设计论述。

## 命名（用户已确认，勿改）

| 对象 | 名称 |
|---|---|
| 仓库目录 | `translator_core/` |
| Rust crate（package） | `translator-core` |
| Rust lib / 产物 | `translator_core` → `translator_core.dll` / `.dylib` / `.so` |
| 产品显示名 | Translator |

命名规则：**核心库用 `_core` 后缀**（`translator_core`），表示它是引擎层
而非主程序。主程序/UI 层另起名，不用 `_core`。
Flutter 侧 `use translator_core::...`，FFI 绑定指向 `translator_core.dll`。

## 项目是什么

跨平台**完全离线**实时音频转写 + en→zh 翻译。目标平台：
Windows / macOS / Android / **鸿蒙卓易通容器**（复用 Android APK）。

硬约束：运行期零网络请求。代码层面不引入任何 HTTP 客户端依赖。

## 技术栈

| 层 | 选型 | 版本 |
|---|---|---|
| 核心 | Rust | 1.99.1（MSVC target，`x86_64-pc-windows-msvc`） |
| 核心依赖 | `aho-corasick` / `unicode-normalization` / `rtrb` / `notify` | 见 `core/Cargo.toml` |
| UI | Flutter + `flutter_rust_bridge` | 待接入 |
| ASR | sherpa-onnx 流式 Zipformer int8 | Apache-2.0 |
| MT | **Bergamot**（Marian NMT）| Mozilla MPL-2.0 |
| 构建 | Cargo + CMake（Bergamot 源码编译）| — |

**依赖策略：核心 crate 零重依赖。** 词库解析等可选路径自带最小实现
（见 `glossary/parse/json.rs` 的手写 JSON parser），不引入 serde/regex。
需要新依赖前先问：能否用 std 实现？

### ★ 翻译引擎选型：为什么是 Bergamot 而非 CTranslate2

两者同为 Marian 框架，**权重与分词器完全复用**，翻译质量无差异。
决定性差别在构建方式：

| | CTranslate2 (Rust 绑定 2.1.1) | **Bergamot（选定）** |
|---|---|---|
| 构建 | `ctranslate2-sys` **联网下载**预编译包 | 纯 CMake 源码编译 |
| 离线构建 | ❌ 实测代理 502 即失败 | ✅ 仅需 C++17 |
| 分词器 | 收 token 级输入，**需自接 SPM** | **收原始文本**，内置 SPM |

曾尝试 CTranslate2，其 build-dependency 引入 `ureq`/`rustls`/`http`，
被 `scripts/check_offline.py` 门禁拦下 —— 这是门禁实际生效的证据。

**离线性是构建门禁，不是口头承诺**：
```bash
python scripts/check_offline.py    # 分别检查 --edges normal 与 --edges build
```


## 命令

```bash
export PATH="$HOME/.cargo/bin:$PATH"     # Git Bash 必须先 export

cd core
cargo build                              # 编译
cargo test                               # 全部测试
cargo test <substring>                   # 单测（优先用这个，见"边界"）
cargo test 2>&1 | grep -E "^error" -A 5 # 编译错误（只看错误，不要全量日志）
cargo build 2>&1 | tail -3               # 只看结果行
```

**不要用 `cargo test --offline`**：依赖未预取，会直接失败。

## 代码约定

### 1. 平台差异只允许出现在 `platform/` 下

**`core/` 内禁止出现任何 `#[cfg(target_os)]`**（除非在 `platform/` 目录内）。
平台差异收敛到 4 个 trait：`SystemAudioCapture` / `PermissionGate` /
`OverlayWindow` / `ConfigStore`。

业务逻辑里出现平台分支 = 抽象漏了，去补 trait。

### 2. 线程安全是硬要求

`GlossarySnapshot` / `GlossaryMatcher` 必须 `Send + Sync`。
**不要用 `thread_local!` 注入数据** —— 已因此返工过一次。
需要上下文数据就作为字段自持（matcher 持有 `entry_targets: Vec<String>`）。

### 2b. sherpa-onnx 接口（**以编译器实测为准**）

⚠️ **docs.rs 的 HTML 摘要页签名渲染有误**，照它写会编译失败。三处实测差异：

| docs.rs 摘要页显示 | 编译器实测 |
|---|---|
| `create(&[OnlineRecognizerConfig])` | `create(&OnlineRecognizerConfig)` — **单值** |
| `create_stream_with_hotwords(&[&str])` | 收**单个** `&str`，格式 `"词1 : 权重  词2 : 权重"` |
| `decode(&[OnlineStream])` 等 | 各方法收**单个** `&OnlineStream` |

实测可用签名：
```text
OnlineRecognizer::create(&config) -> Option<Self>
OnlineRecognizer::create_stream(&self) -> OnlineStream
OnlineRecognizer::create_stream_with_hotwords(&self, hotword: &str) -> OnlineStream
OnlineRecognizer::{decode, is_ready, is_endpoint, reset, get_result}(&self, &stream)
OnlineStream::accept_waveform(&mut self, sample_rate: i32, samples: &[f32])
OnlineStream::input_finished(&mut self)          // 无返回值
RecognizerResult { text, tokens, timestamps, segment, start_time, is_final }
```

**教训**：外部文档一律降级为"待核验"，实际签名以 `cargo build` 报错为准。
这正是"行动前需核验"信任等级的意义。

**热重载**：`create_stream_with_hotwords` 每流独立热词，词库变更时新建 session 即可，**不重建 recognizer**（省 2~3 秒）。

### 3. 实时路径绝不能阻塞

捕获线程 → `AudioChannel`。缓冲满时**丢帧并计数**，绝不阻塞。
阻塞在 Windows 上会被判定为音频故障，在 macOS 上触发系统级中断。

### 4. 归一化必须与原文坐标对齐

`Entry::match_key()` 只做 NFKC + 大小写折叠，**不折叠空白**。
原因：匹配结果要用来切片原文并替换成哨兵，空白折叠会改变字节数，
导致坐标错位（已踩坑：`"Ku▁X0Xsterday"`）。

去重/合并用 `Entry::storage_key()`（它才折叠空白）。

### 5. 注释写"为什么"，不写"是什么"

尤其是反直觉的设计决策，必须写明原因与代价。
判断标准：半年后的人看到这行会不会想改掉它？

## 边界

- **绝不**在 `core/` 业务逻辑写平台分支
- **绝不**让快照在处理中途被换掉 —— 哨兵索引语义会错位，必须用 `SnapshotGuard`
- **绝不**用 `thread_local!` 传递跨线程数据
- **绝不**在阻塞/实时路径上加 `unwrap()`（除非已证明不会失败）
- **绝不**引入任何网络依赖（HTTP 客户端 / TLS / 在线模型库 / 遥测）——
  构建期与运行期皆然。`scripts/check_offline.py` 是门禁，不是参考
- **绝不**做模型在线下载；权重由用户导入 `.dshm` 包
- **绝不**提交模型权重文件到仓库（只有 manifest）
- **跨 FFI 不得 panic**：用 `catch_unwind` 包住并转错误码（`panic = "unwind"`）
- **改测试断言前先问**：是实现错了，还是我的预期错了？
  失败里既有真 bug（大写被破坏、词典取最长匹配）也有错断言
  （rtrb 容量是块数、源文是 `deploy` 而非 `deployed`）
- 目标：每任务聚焦上下文 < 2000 行；不要整篇读 DESIGN.md，只读当前相关小节

## 模式示例

### 模式 1：哨兵往返（本项目核心机制）

```rust
let guard = manager.guard();              // 固定快照，热重载不影响本片段
let pending = guard.start(source_en);

// 内部已把 Kubernetes 换成 ▁X0X
let mt_input = pending.text_for_translation();

// Marian 原样透传哨兵 → 还原为词库指定的译法
let (zh, restore) = pending.finish(mt_output);

if !restore.is_consistent() {
    // 哨兵被吞 → 触发 Layer C 正则兜底 + UI 标记待复核
    flag_for_review(line_idx);
}
```

### 模式 2：测试命名与结构

```rust
#[test]
fn 描述行为而非实现() {
    // Arrange
    let entries = vec![mk("Kubernetes", "Kubernetes", vec![])];
    let m = GlossaryMatcher::build(&entries);
    // Act
    let r = m.protect("We deployed Kubernetes");
    // Assert
    assert_eq!(r.text, "We deployed \u{2581}X0X");
}
```

### 模式 3：配置调参的落点

参数一律走 `config::AppConfig` 的方法，平台实现不得硬编码。
例：`config.asr_threads(cores)` 内部按 `android_runtime` 分支
（容器降到 `cores/3`），调用方无需关心平台。

## 运行时环境（重要）

| 平台 | 内放采集 | 权限 | 坑 |
|---|---|---|---|
| Windows | WASAPI loopback | 免权限 | 需处理设备热插拔 |
| macOS | ScreenCaptureKit | TCC 屏幕录制，需手动授予 | 必须 13.0+；Release 必须签名否则反复掉权限 |
| Android 原生 | AudioPlaybackCapture | MediaProjection 每次冷启动重授 | DRM 内容必然静音 |
| **卓易通容器** | **不可用（按此设计）** | 容器内逐项授权 | **运行时探测 + 必须显式降级，禁止静默** |

`AndroidRuntime::{Native, ZhuoYiTong}` 驱动线程数（容器 `cores/3`）、模型档位（禁 large）、存储路径（禁外部存储）。

**卓易通已确认约束**（2026-10-16 调研，勿再重复调研）：

1. **ABI 校验 → 可能启动即闪退**：容器检查 `.so` 是否有非标准 ABI 调用或未授权反射，命中发 `SIGKILL`。**风险最高，若不通过整个卓易通方案作废。**
2. **Manifest 权限白名单**：`FOREGROUND_SERVICE_MEDIA_PROJECTION` 等敏感权限会被审计，不达标则切断系统 API。
3. 存储与宿主物理隔断 → 352 MB 模型包只能经"文件互传"导入
4. 容器默认休眠、用完进程销毁 → 需前台服务 + 断流自检
5. 黑白名单针对"已有 HAP 原生版"的应用 —— 本产品无原生版，不冲突

## 当前状态（2026-10-16）

- 已编译通过；测试 99/105 通过
- 已知失败：`sentinel` 坐标映射相关（正在修）、`yaml` groups 解析
- 下一步：修 6 个失败测试 → 接 sherpa-onnx ASR 后端

## 待验证（不可仅凭文档结论）

见 `docs/DESIGN.md §12`。最重要的是卓易通容器内
`AudioPlaybackCapture` 是否真的能读到宿主播放流 —— 需真机实测。
