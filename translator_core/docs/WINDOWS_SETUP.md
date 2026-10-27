# Windows 桌面版构建说明

> 目标平台：**Windows 10 及以上，64 位（x64）**。
> 依据：Flutter 3.x 桌面端官方不再支持 Windows 7/8；
> 本项目的音频采集走 WASAPI loopback，需要 Windows 10。

---

## 一、前置依赖

| 依赖 | 本机实测位置 | 说明 |
|---|---|---|
| Visual Studio 2022 | `C:\Program Files\Microsoft Visual Studio\2022\Professional` | 必须勾选**「使用 C++ 的桌面开发」**工作负载 |
| MSVC 编译器 | `...\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\cl.exe` | 随上面工作负载安装 |
| CMake | `...\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe` | Flutter 会自动用它，无需单独装 |
| Windows SDK | 随 VS 安装 | 提供 `kernel32.lib` / `dwmapi.lib` 等 |
| Rust (msvc) | `~/.cargo/bin` | 目标 `stable-x86_64-pc-windows-msvc` |
| Flutter | `D:\flutter\flutter` | 已 `flutter config --enable-windows-desktop` |

检查 Flutter 侧识别情况：

```bash
flutter doctor -v   # 看 "Visual Studio - develop Windows desktop applications" 是否打勾
```

---

## 二、构建（三步）

```bash
cd translator_core

# 1) 编译 Rust 核心 → ui/windows/rust/translator_core.dll
bash Windows/scripts/build_windows.sh            # 默认 release
bash Windows/scripts/build_windows.sh --debug    # 需要带调试符号时

# 2) 修复插件目录联接（重要，见 §4.1）
powershell -ExecutionPolicy Bypass -File Windows/scripts/windows_plugin_junctions.ps1

# 3) 构建桌面程序
cd ui
flutter build windows --release
```

产物：`ui/build/windows/x64/runner/Release/translator.exe`
（`translator_core.dll` 与 `flutter_windows.dll` 会被装在同目录）

### 一次跑完

```bash
bash Windows/scripts/build_windows.sh && \
powershell -ExecutionPolicy Bypass -File Windows/scripts/windows_plugin_junctions.ps1 && \
cd ui && flutter build windows --release
```

---

## 三、目录职责

```
core/                          Rust 核心（平台无关）
  src/platform/desktop.rs      WASAPI loopback / 麦克风采集（flexaudio）
ui/
  lib/ffi/bridge.dart          手写 dart:ffi 绑定
  windows/                     Flutter Windows 运行器
    rust/translator_core.dll   ← build_windows.sh 的产物（构建输入）
    CMakeLists.txt             负责把上面那个 DLL 装到 exe 同目录
  build/windows/x64/runner/Release/
    translator.exe             ← 最终产物
    translator_core.dll        ← 与 exe 同目录，Dart 才能加载到
```

**为什么 DLL 要单独构建而不是 CMake 里编 Rust**：保持与 Android 侧一致
（`Android/scripts/build_android.sh` 预编译 `.so`），也避免 MSVC 交叉编译
Rust 时的工具链探测问题。

---

## 四、踩过的坑（重要）

### 4.1 插件目录联接必须手动修

**现象**（两个连环报错）：

```
CMake Error at flutter/generated_plugins.cmake:22 (add_subdirectory):
  add_subdirectory given source "flutter/ephemeral/.plugin_symlinks/jni/windows"
  which is not an existing directory.
```

紧接着下一次构建：

```
PathExistsException: Cannot create link, path = '...\ephemeral\.plugin_symlinks\file_picker'
(OS Error: 当文件已存在时，无法创建该文件。, errno = 183)
```

**根因**：Flutter 用 `dart:io` 的 `Link.createSync` 在
`ui/windows/flutter/ephemeral/.plugin_symlinks/<插件名>` 建符号链接。
在**非管理员 + 未开启开发者模式**的 Windows 上，这一步会**静默退化成空目录**：

- 空目录 → CMake `add_subdirectory` 找不到 `windows/` 子目录
- 再下一轮构建时，`Link.existsSync()` 对「目录」返回 `false`
  （它只认链接），Flutter 于是再次尝试创建 → 撞上已存在的路径 → errno 183

**解决**：`Windows/scripts/windows_plugin_junctions.ps1` 用 NTFS **junction**
（目录联接）替换符号链接。junction 不需要管理员权限、不需要开发者模式，
且对 `Link.existsSync()` 与 CMake 都是透明的。

> 若 `flutter clean` 或删除 `windows/flutter/ephemeral` 后，需重跑该脚本。

### 4.2 不要引入 `flutter_rust_bridge`

`flutter_rust_bridge` 会传递依赖 `jni` 插件（Android 专用的 JNI 绑定）。
它本身有 `windows/` 目录所以不会立刻报错，但会：
- 让 Windows 构建多编译一份 Android 专用原生代码
- 在 `--debug` 下拖慢构建

本项目 `ui/lib/ffi/bridge.dart` 是**手写 `dart:ffi` 绑定**，
不需要 FRB，故已在 `pubspec.yaml` 中移除。

> 若将来改用 FRB 生成绑定，需确认其 `jni` 依赖在 Windows 下的行为。

### 4.3 `DynamicLibrary.process()` 在 Windows 上会拿不到符号

`bridge.dart` 原本的顺序是「先 `process()`，失败再 `open('translator_core.dll')`」。
这是错的：`DynamicLibrary.process()` **只能解析已被进程加载的 DLL 的符号**。
我们是把 DLL 放在 exe 同目录、并没有在启动时链接它，
所以 `process()` 会成功返回句柄，但随后的 `lookupFunction` 抛
`Failed to lookup function 'translator_*'` —— 报错完全指不到真因。

**正确顺序**：先 `DynamicLibrary.open('translator_core.dll')`
（Windows 加载器搜索顺序里 exe 所在目录在最前，裸文件名即可命中），
失败再退 `process()`。

### 4.4 构建必须带镜像环境变量

```bash
export PUB_HOSTED_URL="https://pub.flutter-io.cn"
export FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn"
```

漏掉时 `flutter build` 会在「Resolving dependencies」长时间挂住
（官方源在国内不稳定）。已写入 `~/.bashrc`。

### 4.5 镜像写进了 Gradle，不适用于桌面端

`android/settings.gradle.kts` 里的阿里云 / Flutter 镜像配置是 **Android 专属**。
Windows 桌面端的 C++ 依赖由 VS + CMake 解析，不走 Gradle，不受影响。

---

## 五、当前能力边界

| 能力 | 状态 |
|---|---|
| 启动、UI（字幕/词库/模型/设置四个页） | ✅ 可用 |
| 加载 `translator_core.dll` + FFI 符号解析 | ✅ 已验证（14/14） |
| 系统内放采集（WASAPI loopback） | ✅ 后端已编译接入，需真机运行确认 |
| 麦克风降级采集 | ✅ 同上 |
| **ASR 转写**（sherpa-onnx） | ⚠️ 未启用 —— `asr-sherpa` feature 默认关闭，需模型权重 |
| **MT 翻译**（Bergamot/Marian） | ⚠️ 未启用 —— `mt-bergamot` feature 默认关闭，需模型权重 |
| 字幕上屏链路 | ⚠️ FFI 已通，但 Rust 侧未把字幕推给 Dart（见 §6） |
| 桌面端全透明置顶悬浮窗 | ❌ 未实现（`OverlayWindow` trait 只有契约，无 Windows 实现） |

> 也就是说：**Windows 版现在是一个能启动、能采集、能显示 UI 的完整外壳，
> 但还出不了文字** —— 因为 ASR/MT 后端与模型包都还没接。
> 下一步就是攻这两块。

---

## 六、下一步建议（按优先级）

1. **补 FFI 流水线接口**：`translator_push_audio` / `translator_poll_result`，
   把「采集 → VAD → ASR → MT → 字幕」串起来，并在 Dart 侧轮询。
   这是让 Windows 版真正出字幕的最小改动。
2. **启用 ASR/MT 后端**：`bash Windows/scripts/build_windows.sh --with-asr`
   （sherpa-onnx 需预编译原生库），以及 `mt-bergamot`。
3. **模型权重**：`scripts/build_model_pack.py` + `models/manifest/minimal.json`。
4. **桌面悬浮窗**：用 `windows/runner/` 的 Win32 代码实现
   `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT`，
   比引入 `window_manager` 插件更可控（且不拖新依赖）。

---

## 七、非 ASCII 路径

工程位于 `C:\Users\Admin\WorkBuddy\代码开发\...`，含中文。
实测 **Windows 侧 CMake/CMake 构建正常**（Android 侧则需要
`android.overridePathCheck=true` 才能过 AGP 检查）。
若日后遇到诡异路径问题，可把工程挪到纯 ASCII 路径下。