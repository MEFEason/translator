// Translator Android 工程设置（Kotlin DSL）
//
// ## 为什么用 KTS 而非 Groovy
// 本工程在 Groovy DSL 上连续踩坑 4 次：
//   1. pluginManagement 必须位于最前（我把 flutterSdkPath 定义放在它前面 → 失败）
//   2. includeBuild 放 plugins 之后 → "No included builds contain this plugin"
//   3. `run {}` 在 pluginManagement 的 Groovy 上下文里不存在 → 失败
// 官方 `flutter create` 生成的就是 KTS，结构已验证可用，故直接采用。
//
// ## 镜像配置依据（2026-10-17 实测，非推测）
// | 源类型 | 可用镜像 | 实测速度 |
// |---|---|---|
// | Maven 依赖（AGP/Kotlin） | 阿里云 ✅ | 8.1 MB/s（官方 4.6 MB/s） |
// | **SDK 包**（platform/NDK/build-tools） | **无** ❌ | 官方 4.6 MB/s 直连可用 |
// | Gradle 发行版 | **腾讯云** ✅ | 官方重定向 GitHub 实测卡死 23 分钟 |
//
// ⚠️ 阿里云/清华/USTC 的 android/repository/ 已不再镜像 Android SDK（404），
// 但 maven.aliyun.com/repository/google 仍完整镜像 Google Maven，两者不是一回事。

pluginManagement {
    val flutterSdkPath =
        run {
            val properties = java.util.Properties()
            file("local.properties").inputStream().use { properties.load(it) }
            val flutterSdkPath = properties.getProperty("flutter.sdk")
            require(flutterSdkPath != null) { "flutter.sdk not set in local.properties" }
            flutterSdkPath
        }

    // Flutter Gradle 插件（含 flutter-plugin-loader，负责生成
    // GeneratedPluginRegistrant）。缺这段 app 模块的 flutter 源集不被识别。
    // ⚠️ 必须在 pluginManagement 内，否则插件解析时找不到
    includeBuild("$flutterSdkPath/packages/flutter_tools/gradle")

    repositories {
        // ── Maven 依赖：阿里云优先（实测比官方快 75%）──
        maven { url = uri("https://maven.aliyun.com/repository/gradle-plugin") }
        maven { url = uri("https://maven.aliyun.com/repository/google") }
        maven { url = uri("https://maven.aliyun.com/repository/public") }
        gradlePluginPortal()

        // ── 官方兜底 ──
        google()
        mavenCentral()
    }
}

plugins {
    // ⚠️ 2026-10-26 修复：原写法是 `apply false` —— 那会让
    // flutter-plugin-loader **不被应用**，插件子工程不会被 include 进构建，
    // 结果 GeneratedPluginRegistrant.java 引用
    //   com.mr.flutter.plugin.filepicker / io.flutter.plugins.* / com.github.dart_lang.jni
    // 时报「程序包不存在」（javac 5 个错误）。
    // 官方 flutter create 模板就是**直接 apply**（无 apply false），
    // 它读 .flutter-plugins-dependencies 把各插件工程 include 进来。
    id("dev.flutter.flutter-plugin-loader") version "1.0.0"
    // ⚠️ AGP 9.1.0 与 Flutter 3.47 模板一致。
    // 降级到 8.x 会报 "only the new DSL interface will be read"。
    id("com.android.application") version "9.1.0" apply false
    // ⚠️ 必须显式声明且版本 ≥ 2.2.20（Flutter 3.47 的最低要求）。
    // 不声明会用 AGP 内置的 2.2.10，报：
    //   "Your project's Kotlin version (2.2.10) is lower than Flutter's
    //    minimum supported version of 2.2.20"
    // 官方 flutter create 模板用的是 2.4.0，此处对齐。
    id("org.jetbrains.kotlin.android") version "2.4.0" apply false
}

dependencyResolutionManagement {
    // ⚠️ 不能用 FAIL_ON_PROJECT_REPOS：
    // Flutter Gradle 插件自身会添加一个 maven 仓库（Flutter 引擎产物），
    // 严格模式会直接报错：
    //   "repository 'maven' was added by plugin 'dev.flutter.flutter-gradle-plugin'"
    //
    // PREFER_SETTINGS：这里的仓库优先，但允许插件追加自己的。
    // 既保证镜像生效，又不阻断 Flutter 插件。
    repositoriesMode.set(RepositoriesMode.PREFER_SETTINGS)
    repositories {
        maven { url = uri("https://maven.aliyun.com/repository/google") }
        maven { url = uri("https://maven.aliyun.com/repository/public") }
        // Flutter 引擎产物（Flutter 官方国内镜像）
        maven { url = uri("https://storage.flutter-io.cn/download.flutter.io") }
        google()
        mavenCentral()
    }
}

rootProject.name = "translator"
include(":app")
