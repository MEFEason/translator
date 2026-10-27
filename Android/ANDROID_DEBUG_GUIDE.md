# Android 端打包调试指南（阶段1：ASR 真机跑通）

> 状态：**构建链路已全部打通、产物门禁通过**；真机验证需你插上手机。
> 阶段1 = 只做语音转写（字幕显示识别原文）；翻译（MT）在阶段2 接入。

## 一、当前产物

| 项 | 位置 |
|---|---|
| APK | `translator_core/ui/build/app/outputs/flutter-apk/app-debug.apk`（约 124 MB） |
| Rust 核心 | `translator_core/ui/android/app/src/main/jniLibs/arm64-v8a/libtranslator_core.so`（1.2 MB） |
| sherpa 运行库 | 同目录 `libsherpa-onnx-c-api.so` + `libonnxruntime.so` |
| 交叉编译脚本 | `Android/scripts/build_android.sh` |
| 部署脚本 | `Android/scripts/deploy_device.sh` |
| 产物门禁 | `Android/scripts/verify_android_product.py` |

APK 里 arm64-v8a 三库齐全，权限（RECORD_AUDIO / FOREGROUND_SERVICE）已声明，
采集前台服务与主 Activity 均在 manifest 中。

## 二、真机部署（三步）

### 1. 连手机并授权
- 手机开启「开发者选项 → USB 调试」
- **用数据线**连接（充电线无法 adb）
- 手机弹出「允许 USB 调试吗」→ 勾「始终允许」→ 确定
- Windows 侧若 `adb devices` 显示 `unauthorized`，装厂商 USB 驱动
- 验证：
  ```bash
  "D:/Android/Sdk/platform-tools/adb.exe" devices -l
  ```
  看到 `device`（不是 unauthorized/offline）即可。

### 2. 一键部署
```bash
cd C:/Users/Admin/WorkBuddy/代码开发/Translator
bash Android/scripts/deploy_device.sh
```
该脚本会：安装 APK → 推送 ASR 模型（162 MB）到
`/sdcard/Android/data/com.translator.app.debug/files/models/` → 启动应用 → 打印 logcat 摘要。

推 MT 权重（阶段2 用）：`bash Android/scripts/deploy_device.sh --mt`
只装不推模型：`bash Android/scripts/deploy_device.sh --no-push`

> 调试包名带 `.debug` 后缀（`applicationIdSuffix`），推送路径已按此推导。

### 3. 在手机上验证
1. 打开应用 → 授予「麦克风」权限（首次会弹窗）
2. 点「开始转写」→ **对着手机说英文**
3. 预期：**2 秒内**出现流式原文预览，说完定句后进入字幕列表
4. 转写盘文件落在 `Android/data/com.translator.app.debug/files/translate results/`
5. 顶部会显示「翻译引擎未启用：未安装翻译权重，当前仅显示识别原文」
   —— 这是阶段1 的**预期状态**，不是故障

抓日志：
```bash
adb logcat -s TranslatorCapture:* TranslatorBridge:* flutter:* libc:*
```

## 三、Android Studio 调试

**打开工程**：`File → Open` → 选 `translator_core/ui/android`（**不要**选上层目录）

- Gradle 9.3.1 / AGP 9.1.0 / Kotlin 2.4.0 / NDK 30.0.16248370
- `local.properties` 已配好 `sdk.dir=D:/Android/Sdk`、`flutter.sdk=D:\flutter\flutter`
- 若 AS 报 JAVA_HOME 问题：AS 自带 JBR，一般无需配置

**改动与重编的对应关系**（重要）：

| 改了什么 | 必须做什么 |
|---|---|
| Dart 代码（`ui/lib/**`） | 热重载即可（Debug → Run 后改代码按 `r`） |
| Kotlin 代码（`ui/android/app/src/main/kotlin/**`） | 重新 Run（30–60 秒） |
| Rust 代码（`translator_core/core/**`） | **重跑 `build_android.sh` + 重新 Run**（.so 换了，符号地址变） |
| C++ 桥（`ui/android/app/src/main/cpp/**`） | 重新 Run（CMake 会重编） |
| pubspec 依赖 | `flutter pub get` + 重新 Run |

Rust 改动重编命令：
```bash
cd C:/Users/Admin/WorkBuddy/代码开发/Translator
TRANSLATOR_SEMVER=0.9.27 TRANSLATOR_CHANNEL=beta bash Android/scripts/build_android.sh --release
```

## 四、模型分发说明

模型**不进 APK**（162 MB 会让安装极慢），用 adb push 送到应用外部私有目录：

```
/sdcard/Android/data/com.translator.app.debug/files/models/
├── asr/
│   └── sherpa-onnx-x-asr-960ms-streaming-zipformer-transducer-zh-en-punct-int8-2026-06-05/
│       ├── encoder.int8.onnx / decoder.onnx / joiner.int8.onnx
│       └── tokens.txt / bpe.vocab
├── mt/          ← 阶段2 再推
└── glossary/    ← 可选
```

Dart 侧会自动定位 `asr/` 下的具体模型目录（Rust 的 `probe_asr_dir` 只看本层，
模型文件在下一层，不能直接传 `models/asr`）。

## 五、屏幕适配（手机锁竖屏 / 平板可旋转）

### 策略

| 设备 | 屏幕方向 | 导航形态 | 正文宽度 |
|---|---|---|---|
| 手机（sw < 600dp） | **锁竖屏** | 底部导航栏 | 铺满全宽 |
| 平板（sw ≥ 600dp） | **可旋转**（尊重系统自动旋转开关） | 侧边导航栏 | 竖屏 840dp / 横屏 960dp，居中 |
| 桌面（Windows） | 用户自拖窗口 | 底部导航栏 | 铺满全宽（保持既有观感） |

### 实现要点

**方向策略在运行时判定**，不在 Manifest 里写死：

```kotlin
// ui/android/app/src/main/kotlin/com/translator/app/OrientationPolicy.kt
val want = if (isTablet(config)) SCREEN_ORIENTATION_FULL_USER   // 13 平板可旋转
           else                   SCREEN_ORIENTATION_PORTRAIT    //  1 手机锁竖屏
```

- Manifest 只声明基线 `android:screenOrientation="fullUser"`（值 13），
  运行时再按设备类型收紧 —— 静态声明没法区分手机/平板。
- 平板判定用 `smallestScreenWidthDp >= 600`（Android 官方的 sw600dp 口径）。
  它是**设备**属性：手机分屏时不变（不会误判成平板），
  折叠屏展开时 320→673 自动切成平板策略。

### 旋转不会打断转写

Manifest 的 `configChanges` 含
`orientation|screenSize|smallestScreenSize|screenLayout`
→ **Activity 不重建**，只重排。重建会丢 Rust 会话指针，
正在进行的转写会全部中断。这是本项目的硬要求，别把这些 flag 删掉。

### 平板与折叠屏

- 平板：左边 `NavigationRail`，右边内容区。
- 折叠屏：折叠态按手机（锁竖屏 + 底栏），展开后自动切平板（可旋转 + 侧栏）。
- 判定变化由 `ResponsiveScope` 监听**窗口短边**变化触发 → 重新向 Kotlin
  询问设备画像（旋转不触发，因为短边不变）。

### 手动验证

```bash
# 手机：应锁竖屏，横屏无效
adb shell settings put system accelerometer_rotation 0
adb shell am start -n com.translator.app/.MainActivity

# 查当前窗口尺寸（dp）→ 判断是否被当成平板
adb shell dumpsys window displays | grep -E "cur=|init="
```

判定为平板时 `dumpsys` 里的宽度会 ≥ 600dp。

---

## 六、已知限制（阶段1）

| 限制 | 说明 |
|---|---|
| 无翻译 | MT 权重未安装，字幕只有原文。UI 有明示提示条 |
| 无独立字幕窗 | 桌面端的「第二实例悬浮窗」依赖 Win32，Android 上未实现 |
| 无系统托盘 | 同上（Android 用前台服务通知替代） |
| 无文件选择器导出 | `file_picker` 的 `saveFile` 在 Android 不支持，导出改为写应用私有目录 |
| 卓易通容器未验证 | 容器内 MediaProjection 需宿主代理，代码里已做降级路径，但未实测 |
| 仅 arm64 | 按卓易通容器要求只保留 arm64-v8a（模拟器需另编 x86_64） |

## 七、常见问题

**Q：构建报「Building with plugins requires symlink support」**
```powershell
powershell -ExecutionPolicy Bypass -File Windows\scripts\plugin_junctions.ps1 -Platform android
```
（非管理员 + 未开开发者模式时，符号链接会失败；用 NTFS junction 替代）

**Q：Gradle 报 `JAVA_HOME is not set`**
```bash
export JAVA_HOME="C:/Program Files/Android/Android Studio/jbr"
```

**Q：CMake 报「找不到 libtranslator_core.so」（但文件明明在）**
先跑 `build_android.sh`；若已存在仍报错，是路径含中文导致 `find_library` 失效
——已改用 `IMPORTED SHARED` 规避，若再出现请检查 CMakeLists.txt 是否被还原。

**Q：应用启动即崩**
多半是 `libonnxruntime.so` / `libsherpa-onnx-c-api.so` 没进 APK。
用 `aapt2 dump xmltree` 之外的方式查：
```bash
unzip -l app-debug.apk | grep jniLibs
```

**Q：麦克风没声音 / 没字幕**
```bash
adb logcat -c && adb logcat -s TranslatorCapture:* flutter:* libc:*
```
看 `[mt] 译文为空`（阶段1 正常）、`[supervisor] 后端就绪`（引擎起来了）、
`capture_rms`（诊断页看 RMS，恒 0 说明没采到声音）。
