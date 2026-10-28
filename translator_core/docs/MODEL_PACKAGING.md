# 模型权重打包说明

> 2026-10-28 用户要求：提交 git 前先写明「模型权重已打包进包内」。

---

## 一、结论

**Android 端的全部模型权重（ASR + MT + 离线精修 + 词库）已随 APK 一起分发。**

用户安装 APK 后**无需任何额外操作**即可获得完整的「识别 + 翻译」能力：
不需要下载模型、不需要 adb push、不需要手动放文件到某个目录。

| 模型 | 用途 | 体积 | 是否随包 |
|---|---|---|---|
| `models/asr/` | sherpa-onnx 流式识别（中英双语 zipformer） | ~162 MB | ✅ |
| `models/mt/` | Bergamot 离线翻译（Marian intgemm 权重） | ~50 MB | ✅ |
| `models/offline/` | 第二遍解码精修（SenseVoice） | ~229 MB | ✅ |
| `models/glossary/` | 术语词库（`roger-courses.tsv`） | ~1 MB | ✅ |
| **合计** | | **~441 MB** | ✅ |

---

## 二、机制

### 2.1 打包阶段（Gradle）

`translator_core/ui/android/app/build.gradle.kts` 里的 `syncModels` 任务
（挂在 `preBuild` 上，任何走 assets 打包的任务都会先跑它）：

- 把 `translator_core/models/` 镜像到 `android/app/src/main/assets/models/`
- 剔除 `README.md` / `manifest/` / `sources.json`（文档与元数据，不参与运行）
- **二进制文件全部 `noCompress`** —— ONNX 权重、`.bin`、`.spm` 本来就难压缩，
  再压一遍只是白费 CPU 且几乎不减小体积

### 2.2 首启解压（Kotlin）

`ModelExtractor.kt` 在首次启动时把 assets 里的模型**流式解压**到
`filesDir/models`：

- **流式**（不整体读进内存）—— 441MB 一次性读入会 OOM
- **后台线程**执行 —— 绝不能在 MethodChannel 主线程同步解压（会 ANR）
- 完成后写标记文件 `models/.extracted`（内容 = versionName）
- **标记最后写** —— 中途被杀不会留下「看起来已完成」的半成品标记
- 已有标记 → 直接跳过（幂等，升级不重复解压）

### 2.3 Dart 侧等待

`translator_state.dart` 的 bootstrap 流程：

```
waitForModels()  ←─ 轮询 modelsStatus 直到 extracting=false
       ↓
_probeAndroidModels()  ←─ 探测 models/asr、models/mt
       ↓
sessionCreate(cfg)     ←─ cfg 里带 asr_model_dir / mt_model_dir
```

⚠️ **必须等解压完成再探测**：否则会探到「目录存在但文件不全」的半成品，
加载必崩（2026-10-27 实测踩过：日志显示「模型目录命中」早于「解压完成」）。

---

## 三、翻译引擎（2026-10-28 接入）

Android 端此前**只有识别、没有翻译** —— 根因是构建 feature 缺 `mt-bergamot`：

```bash
# 修复前
FEATURES="capture-android,asr-sherpa-android"   # ← 没有 mt-bergamot
# 修复后
FEATURES="capture-android,asr-sherpa-android,mt-bergamot"
```

没有该 feature 时，Rust 侧 `build_mt_backend` 走
`#[cfg(not(feature = "mt-bergamot"))]` 分支 → 返回 `NullMt` →
表现为「能识别但译文永远是原文」。

### Bergamot arm64 交叉编译

Bergamot 是 C++ 项目（Marian 权重格式），Windows 端用的是预编译的 MSVC
静态库，Android 需要 arm64 版 → 用 NDK 交叉编译：

```
Android/scripts/build_bergamot_android.sh    ← 交叉编译 + 链接清单导出
Android/scripts/gen_bergamot_libs.py         ← 从 build.ninja 解析依赖图
translator_core/core/build.rs                ← link_android()：读清单 → rustc-link-*
```

产物：`translator_core/third_party/bergamot-translator/build-android/`
- `src/translator/libbergamot-translator.a`（3.7 MB）
- `bergamot_libs.txt`（35 个静态库的链接清单）

### 关键点

- **MT 权重与引擎是两回事**：权重早就在包里（见上表），
  缺的是**能加载它的引擎**。现在两者齐备。
- **`mt_optional` 只在权重缺失时置位**：权重在 → `mt_model_dir` 被设上
  → 走真实翻译路径；`mt_ready` 如实上报可用状态。

---

## 四、Windows 端的对应情况

Windows 端的模型**同样随安装包分发**（`Translator\release\<版本>\models\`），
机制不同但结果一致：

| | Windows | Android |
|---|---|---|
| 模型位置 | 安装目录下的 `models\` | APK assets → 首启解压到 `filesDir\models` |
| 谁负责搬运 | Inno Setup 安装器 | `ModelExtractor`（Kotlin） |
| 用户可见 | 是（资源管理器直接可见） | 否（应用私有目录，通过 APP 内 UI 管理） |

---

## 五、验证方法

**APK 内是否真的带了模型**：

```bash
# 看 APK 里的 assets
7z l build/app/outputs/flutter-apk/app-debug.apk | grep "assets/models"

# 看体积（应在 600MB 量级）
ls -la build/app/outputs/flutter-apk/app-debug.apk
```

**真机上是否已解压到位**：

```bash
adb shell run-as com.translator.app.debug ls files/models/
# 应看到 asr/ mt/ offline/ glossary/ .extracted
```

**翻译引擎是否链进去了**：

```bash
llvm-nm --defined-only \
  translator_core/ui/android/app/src/main/jniLibs/arm64-v8a/libtranslator_core.so \
  | grep bergamot_translator
# 应看到 create / translate / free_string / destroy 四个 C ABI 函数
```

---

## 六、注意事项

1. **APK 体积约 600MB**（模型 441MB + 引擎）。这是「完全离线」的代价，
   是产品硬约束（运行期零网络）的直接结果，不是打包缺陷。
2. **首次启动需要十几秒解压**。UI 会显示「正在安装模型，首次启动约需十几秒…」。
3. **模型不进 git**（`android/app/src/main/assets/.gitignore` 已忽略 `models/`）。
   仓库里只有 `translator_core/models/` 的源，APK 打包时由 Gradle 同步过去。
4. **APK 体积超过 Google Play 的限制**（未压缩 APK 150MB 上限）。
   若将来要上架，需要改用 Play Asset Delivery 或按需下载 ——
   但那会与「完全离线」的定位冲突，需产品层面决策。
