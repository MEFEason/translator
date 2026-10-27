// Translator Android 应用构建脚本（Kotlin DSL + AGP 9）
//
// ⚠️ 本文件采用 **AGP 9 新 DSL**（官方 flutter create 模板的结构）。
// 用 AGP 8.x + 旧 DSL（kotlinOptions / 显式 compileSdk）会报：
//   "Starting AGP 9+, only the new DSL interface will be read"
// 不要降级到旧写法。
//
// ## 关键决策
// 1. debug 构建由 SDK 自动签名，无需 keystore（官方文档确认）
//    → 卓易通安装 APK 时校验签名，debug key 固定可复现
// 2. **compileSdk 36 但 targetSdk 34** —— 两者不同：
//    compileSdk 是编译期用哪个 API；targetSdk 是声明兼容的运行行为。
//    用 36 满足 Flutter 要求，用 34 避开卓易通容器尚未适配的新 API 门控。
// 3. ABI 只保留 arm64-v8a —— 卓易通是 arm64 容器，
//    带其它 ABI 会让 core + sherpa-onnx + Bergamot 各复制一份，包体暴涨。

import java.util.Properties

plugins {
    id("com.android.application")
    id("dev.flutter.flutter-gradle-plugin")
    // 本工程有 Kotlin 源码（AudioCaptureService / MainActivity /
    // NativeBridge / ProjectionPermissionActivity），必须应用此插件。
    // 版本在 settings.gradle.kts 的 plugins 块统一声明（≥ 2.2.20）。
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.translator.app"
    // Flutter 3.47 要求 compileSdk ≥ 36（flutter doctor 会校验）
    compileSdk = 36
    // 与已安装的 NDK 对齐
    ndkVersion = "30.0.16248370"

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "com.translator.app"
        // AudioPlaybackCapture 需要 API 29+
        minSdk = 29
        // 容器审计会比对 targetSdkVersion，34 是兼容性较好的选择
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"

        // 只留 arm64：卓易通容器是 arm64，带其它 ABI 会显著增大包体
        //
        // ⚠️ 2026-10-26 修复：原写法是 `abiFilters += listOf("arm64-v8a")`
        // —— `+=` 是**追加**语义，AGP 9 的默认 ABI 集里仍留着 armeabi-v7a，
        // 于是 Gradle 会为 v7a 也构建 native 桥 → 把 arm64 的 .so 链进
        // armv7 → "is incompatible with armelf_linux_eabi" 链接失败。
        //
        // AGP 9 里 abiFilters 是 MutableSet<String> 且只读，
        // 故用 clear() + add() 替换整个集合。
        ndk {
            abiFilters.clear()
            abiFilters.add("arm64-v8a")
        }
    }

    signingConfigs {
        // debug 由 SDK 自动签名（~/.android/debug.keystore）。
        // Android Studio 与 gradlew 用同一个，故两边产物一致。
        getByName("debug")
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
            isDebuggable = true
            signingConfig = signingConfigs.getByName("debug")
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
        getByName("release") {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            // TODO: 配置正式签名（keystore 不入库）
        }
    }

    // 预编译的 Rust .so（由 scripts/build_android.sh 生成到 jniLibs）
    sourceSets {
        getByName("main") {
            jniLibs.srcDir("src/main/jniLibs")
        }
    }

    // 编译 JNI 桥接层（Kotlin external fun → Rust C ABI）
    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false   // 减小 APK 体积（extractNativeLibs=false）
        }
        resources {
            excludes += "META-INF/*.kotlin_module"
        }
    }
}

// AGP 9 用新的 Kotlin DSL（不再是 kotlinOptions）
kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

// ── Flutter 引擎依赖（显式声明，不依赖插件隐式注入）─────────
//
// ## 为什么显式声明
// AGP 9 下 `dev.flutter.flutter-gradle-plugin` 未把
// `io.flutter:flutter_embedding_debug` 注入到 Kotlin 编译类路径，
// 报 "Unresolved reference 'io.flutter.embedding'"。
//
// ## 版本号的来源
// 坐标是 `1.0.0-<engine_hash>`，hash 来自 Flutter SDK 的
// `bin/internal/engine.version`（随 Flutter 版本固定）。
// 用旧值 1.0.0 会 404 —— 这也是实测踩过的坑。
// local.properties 里有 flutter.sdk（Gradle 插件与引擎依赖都要用）
val localProps = Properties()
val localPropsFile = rootProject.file("local.properties")
require(localPropsFile.exists()) {
    "local.properties 不存在，需含 flutter.sdk 与 sdk.dir"
}
localPropsFile.inputStream().use { localProps.load(it) }

val flutterSdkPath: String = localProps.getProperty("flutter.sdk")
    ?: throw GradleException("flutter.sdk not set in local.properties")

// 引擎版本：坐标为 1.0.0-<hash>，hash 随 Flutter 版本固定。
// ⚠️ 用旧值 1.0.0 会 404（实测踩过）。
val engineVersion: String = file("$flutterSdkPath/bin/internal/engine.version")
    .readText().trim()

dependencies {
    // debug 带 VM 服务（可热重载），release 为 AOT。
    debugImplementation("io.flutter:flutter_embedding_debug:1.0.0-$engineVersion")
    releaseImplementation("io.flutter:flutter_embedding_release:1.0.0-$engineVersion")
}

flutter {
    source = "../.."
}