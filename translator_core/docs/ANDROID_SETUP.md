# Android 环境配置指南

> 面向国内网络环境。所有步骤约 5 分钟。

## 一、SDK 源配置（必做）

### 先说结论：红叉的真实原因不是镜像，是代理

2026-10-17 实测排查发现：SDK Manager 全部源显示红叉，与镜像无关 ——
Android Studio 的代理配置指向了一个**已失效的端口**。

| 项 | 值 | 状态 |
|---|---|---|
| IDE 配置（`proxy.settings.xml`） | `127.0.0.1:**54672**` | ❌ 无监听 |
| 实际代理（环境变量） | `127.0.0.1:**61680**` | ✅ 活跃 |

**第一步：修正代理**（这一步不做，后面全白搭）

`File` → `Settings` → `Appearance & Behavior` → `System Settings` → `HTTP Proxy`
→ 选 **`No proxy`**

> 实测直连 `dl.google.com` 为 200 响应、4.6 MB/s，完全够用。
> 代理端口会随代理软件重启变化，硬编码容易再次失效，故推荐关闭 IDE 代理。

**第二步：添加可用镜像**

进入 `Languages & Frameworks` → `Android SDK` → **`SDK Update Sites`** → `+`

| 字段 | 值 |
|---|---|
| Name | `Tencent` |
| URL | `https://mirrors.cloud.tencent.com/AndroidSDK/` |

然后：
- **勾选** `Tencent`
- **保留勾选** 两个 Google 官方源（作为腾讯云缺包时的兜底）
- **删除或取消勾选** USTC、清华、阿里云 —— 见下

### 关于那些"知名"镜像：实测均已失效

2026-10-17 逐个实测 `repository2-1.xml`：

| 镜像 | 状态 | 说明 |
|---|---|---|
| 阿里云 `mirrors.aliyun.com/android/repository/` | ❌ **404** | 不再镜像 Android SDK |
| 清华 TUNA `mirrors.tuna.tsinghua.edu.cn/android/repository/` | ❌ **404** | 同上 |
| 中科大 USTC `mirrors.ustc.edu.cn/android/repository/` | ❌ **404** | 同上 |
| 腾讯云 `mirrors.cloud.tencent.com/AndroidSDK/` | ✅ **200**（377036 B） | 完整镜像，与官方字节数一致 |
| 官方 `dl.google.com/android/repository/` | ✅ 200（377036 B） | 直连可用 |

⚠️ **网上教程仍在推阿里云/清华/USTC，但这些已不镜像 Android SDK 包**。
注意区分：阿里云的 **Maven** 仓库（`maven.aliyun.com/repository/google`）
仍然有效且更快（实测 8.1 MB/s vs 官方 4.6 MB/s），本工程已在
`settings.gradle` 中配置好。

腾讯云源已验证包含：`repository2-1/2-3.xml`、`addon2-1.xml`、
`android-ndk-r30-*.zip`、`platform-34-*`、`build-tools_r3x_windows.zip`。

### 验证配置生效

重启 Android Studio → `Android SDK` 页 → `SDK Platforms` 标签，
若能看到可安装的平台列表（而非红叉），即已生效。

---

## 二、NDK 安装（Rust 交叉编译必需）

本项目的核心库是 Rust，需要 NDK 才能编译出 Android 的 `.so`。

### 前置

先装 `cmdline-tools`（含 `sdkmanager`）：
1. 设置页 → `SDK Tools` 标签
2. 勾选 **`Android SDK Command-line Tools (latest)`**
3. `Apply`

### 安装 NDK

1. `SDK Tools` 标签 → `Show Package Details`
2. 勾选 **`NDK (Side by side)`** 任意版本（建议 26.x 或 27.x）
3. `Apply`

或用命令行（需先配好 `ANDROID_HOME`）：

```bash
sdkmanager "ndk;26.1.10909125"
```

### 验证

```bash
ls "$LOCALAPPDATA/Android/Sdk/ndk"
```

---

## 三、在 Android Studio 中打开工程

工程已就绪（`ui/android`），直接打开即可：

1. `File` → `Open`
2. 选择目录：`translator_core/ui`（选 **ui** 这一层，**不是** `ui/android`）
3. 首次打开会提示缺少 `gradle-wrapper.jar`，点 **OK** 让 IDE 自动生成

> **为什么选 `ui` 而不是 `ui/android`**
>
> Flutter 工程的根是 `ui`（含 `pubspec.yaml`），
> `android/` 是其平台子目录。选错层 Flutter 插件不会生效。

---

## 四、构建

### 方式 A：Android Studio（推荐，双端同步）

菜单 `Build` → `Build Bundle(s) / APK(s)` → `Build APK(s)`

产物：
```
ui/build/app/outputs/flutter-apk/app-debug.apk
```

debug 包由 SDK 自动签名，已 zipalign，可直接安装。

### 方式 B：命令行

```bash
cd ui
flutter build apk --debug
```

### 方式 C：直接用 Gradle（不依赖 Flutter CLI）

```bash
cd ui/android
gradlew.bat assembleDebug
```

产物：
```
ui/android/app/build/outputs/apk/debug/app-debug.apk
```

---

## 五、镜像配置已内置

工程内的 Gradle 镜像已配好，无需额外操作：

| 位置 | 内容 | 实测 |
|---|---|---|
| `android/settings.gradle` | 阿里云 Maven（AGP / Kotlin / Flutter 插件） | ✅ 8.1 MB/s，比官方快 75% |
| `android/gradle/wrapper/gradle-wrapper.properties` | 官方 Gradle 发行版 | 清华镜像实测 HTTP=000，已改回官方 |

> Gradle 发行版实测：清华 `mirrors.tuna.tsinghua.edu.cn/gradle/` 连接失败，
> 阿里云 `mirrors.aliyun.com/gradle/` 404。官方 `services.gradle.org`
> 会重定向到 GitHub，实测可下载。若 Gradle 卡住，从
> `https://github.com/gradle/gradle-distributions/releases` 手动下载
> `gradle-8.7-all.zip` 放到 `~/.gradle/wrapper/dists/` 对应目录。

---

## 六、常见问题

### 下载 502 / 超时
镜像没生效 → 回第一节重做；或换另一个镜像。

### `Could not find com.android.tools.build:gradle:8.5.2`
`settings.gradle` 的 `pluginManagement` 镜像未生效。
检查是否被 IDE 的 Settings 覆盖了（IDE 设置里的仓库优先）。

### NDK 相关编译失败
NDK 未装 → 回第二节。

### `gradle-wrapper.jar` 缺失
正常。Android Studio 打开工程时会自动补全，
或执行一次 `gradle wrapper`（需已装 Gradle）。

---

## 七、安装到卓易通

1. 取 `app-debug.apk`
2. 传到手机（微信文件传输助手 / 数据线 / 云盘均可）
3. 在**卓易通**内通过「文件互传」或 APK 安装器导入
4. 首次启动会请求权限，**逐项授予**

⚠️ 容器内的注意事项见应用内「设置 → 运行环境 → 卓易通使用须知」。

---

## 八、当前实际状态（2026-10-17 更新）

### 已完成

| 项 | 状态 |
|---|---|
| SDK 位置 | **`D://Android//Sdk`**（已从 C 盘迁移，见 `Android/scripts/update_sdk_paths.sh`） |
| Flutter | **`D://flutter//flutter`** 3.47.7（Dart 3.13.5） |
| Rust target | `aarch64-linux-android` 已装 |
| `libtranslator_core.so` | ✅ 已生成（476 KB，ARM64） |
| 依赖锁定 | ✅ `pubspec.lock` 已生成（60 包） |
| 环境自检 | ✅ `python Android/scripts/check_android_env.py` 全 PASS |

### 一次性环境配置（已写入 `~/.bashrc`）

```bash
export FLUTTER_ROOT="D:/flutter/flutter"
export PUB_HOSTED_URL="https://pub.flutter-io.cn"
export FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn"
export PATH="$FLUTTER_ROOT/bin:$PATH"
alias flutter='D:/flutter/flutter/bin/flutter'
alias dart='D:/flutter/flutter/bin/dart'

export ANDROID_HOME="D:/Android/Sdk"
export ANDROID_SDK_ROOT="D:/Android/Sdk"
export ANDROID_NDK_HOME="D:/Android/Sdk/ndk/30.0.16248370"
export PATH="$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$PATH"
```

另需执行一次（持久化到 Flutter 配置）：
```bash
flutter config --android-sdk D:/Android/Sdk
```

### Flutter 要求的 SDK 组件

`flutter doctor` 实测要求 **SDK 36 + BuildTools 28.0.3**（比工程
`compileSdk = 34` 更新）。已安装：
- `platforms/android-36`、`platforms/android-34`
- `build-tools/28.0.3`、`build-tools/36.0.0`

安装命令（注意是 `android.exe` 不是 `sdkmanager`）：
```bash
D:/Android/Sdk/cmdline-tools/latest/bin/android.exe --sdk=D:/Android/Sdk \
    sdk install "build-tools;28.0.3" "platforms;android-36"
```

### Git Bash 调用注意

`flutter` 无扩展名的是 Unix 脚本，Python 的 `subprocess` 无法 exec
（WinError 193）。脚本里探测可执行文件时必须优先 `.exe` / `.bat` / `.cmd`
（`Android/scripts/check_android_env.py` 的 `find_exe` 已处理）。

### 构建

```bash
cd ui
flutter build apk --debug
# 产物: ui/build/app/outputs/flutter-apk/app-debug.apk
```
