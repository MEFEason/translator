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

import java.util.Calendar
import java.util.Properties
// ⚠️ 脚本里必须**顶部 import**，不能在代码里写 java.util.Calendar 全限定名 ——
// Gradle Kotlin DSL 的 `java` 前缀被 java 扩展（JavaPluginExtension）遮蔽，
// 全限定名会报 "Unresolved reference 'util'"（实测踩过，两次构建失败皆因于此）。

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

        // ── 版本号（2026-10-28 用户规定，与 Rust build.rs 同一格式）──
        //
        // 格式：{端}-{渠道}-v{语义版本}-{YYYY}.{MMDD}.{HH}.{mm}
        // 例：android-beta-v0.9.7-2026.1028.09.35
        // 端标识**全小写**。三段环境变量由 Android/scripts/build_android.sh
        // 注入；缺失时退回 snapshot-v0.0.0 + 当前时间（与 Rust 侧一致）。
        // versionName 允许任意字符串（不含空格），故可直接放完整版本串。
        versionCode = 1
        versionName = translatorVersionName()

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

        // ── release 自签名（2026-10-29）──
        //
        // ⚠️ keystore **不入库**（.gitignore 的 *.p12 生效），
        // 路径与密码在本地 android/keystore.properties（同样不入库）。
        // 换机器构建前先恢复这两个文件，否则 release 构建会
        // 明确报错提示（见下方 error()）。
        // 本项目不上架 Play，签名只为「同签名才能覆盖安装」的升级连续性。
        // 将来若上架 Play，必须改用 Play App Signing 并撤销此密钥。
        create("release") {
            // ⚠️ 用顶部 import 的 Properties，不写 java.util.Properties 全限定名
            //（本文件顶部注释的坑：`java` 前缀被 JavaPluginExtension 遮蔽）
            val ksProps = Properties().apply {
                val f = rootProject.file("keystore.properties")
                if (f.exists()) f.inputStream().use { load(it) }
            }
            val path = ksProps.getProperty("storeFile")
            if (path == null) {
                // 不在配置期 error() —— Gradle **任何任务**都会评估这段
                //（debug 构建也一样），直接抛会让 debug 全挂。
                // 这里只警告 + 指向空配置，真正拦在产物上：
                // 见下方 buildTypes.release 的校验。
                logger.warn("缺 android/keystore.properties —— release 构建将失败（debug 不受影响）")
            }
            path?.let {
                storeFile = rootProject.file(it)
                storePassword = ksProps.getProperty("storePassword")
                keyAlias = ksProps.getProperty("keyAlias")
                keyPassword = ksProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
            isDebuggable = true
            signingConfig = signingConfigs.getByName("debug")
            applicationIdSuffix = ".debug"
            // 2026-10-28：不再追加 "-debug" 后缀 —— 渠道段（beta/snapshot）
            // 已表达发布阶段，再叠 -debug 会让版本串偏离用户规定格式。
        }
        getByName("release") {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            // keystore.properties 缺失时 signingConfig 里 storeFile
            // 为 null → AGP 报一个很难懂的错。提前给出明确指引。
            if (signingConfigs.getByName("release").storeFile == null) {
                error("release 构建需要签名：请恢复 android/keystore.properties " +
                    "与其中的 keystore 文件（见本文件 signingConfigs.release 注释）")
            }
            signingConfig = signingConfigs.getByName("release")
        }
    }

    // 预编译的 Rust .so（由 scripts/build_android.sh 生成到 jniLibs）
    // + 模型 assets（由下方 syncModels 任务从 translator_core/models 拷入）
    sourceSets {
        getByName("main") {
            jniLibs.srcDir("src/main/jniLibs")
        }
    }

    // ── 模型不进二次压缩（2026-10-28 模型内置化）──────────
    //
    // .onnx / .bin 是已压缩或二进制权重，deflate 既慢又省不了多少，
    // 还会让首启解压（assets → filesDir）多一层无谓的 inflate。
    // noCompress 后 assets 以 STORED 存进 APK，读取即拷贝，快得多。
    androidResources {
        noCompress += listOf(
            "onnx", "bin", "spm", "model", "txt", "tsv", "json", "yaml", "yml",
        )
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

// ── 模型同步进 assets（2026-10-28 模型内置化，任务 #15）────
//
// ## 为什么走 Gradle 而不是让开发者手动拷
// 用户明确要求：模型随软件安装，不得要求 adb push。
// 单一事实来源 = translator_core/models（Windows 端同源），
// 构建 APK 时自动镜像到 src/main/assets/models/。
//
// ## 路径推导
// rootDir = ui/android → ../../models = translator_core/models
//
// ## 增量
// Copy 任务按内容指纹判定 up-to-date：模型不变则秒过，
// 变了才重拷（441MB 全量拷贝约 10 秒，可接受）。
//
// ## 剔除清单
// README.md / manifest/ / sources.json 是给桌面安装器看的元数据，
// 不进 APK（运行时只按目录结构探测模型文件）。
val modelsSrc = rootDir.resolve("../../models")
val modelsDst = file("src/main/assets/models")

val syncModels by tasks.registering(Copy::class) {
    group = "translator"
    description = "把 translator_core/models 镜像到 assets/models"
    from(modelsSrc) {
        exclude("README.md", "manifest/**", "sources.json")
    }
    into(modelsDst)
    // 模型目录整体作为输入：任何文件增删改都触发重拷
    inputs.dir(modelsSrc)
    outputs.dir(modelsDst)
    doFirst {
        if (!modelsSrc.exists()) {
            throw GradleException(
                "找不到模型目录 $modelsSrc —— 模型内置化要求 " +
                "translator_core/models 存在（含 asr/、offline/）。" +
                "若确实要出无模型包（仅 mock 链路），用 -x syncModels 跳过本任务。")
        }
    }
}

// 任何构建（assemble/bundle 的公共前置都是 preBuild）先同步模型
tasks.named("preBuild") {
    dependsOn(syncModels)
}

// ── 版本串组装（2026-10-28 用户规定格式）─────────────────
//
// 格式：{端}-{渠道}-v{语义版本}-{YYYY}.{MMDD}.{HH}.{mm}
// 例：android-beta-v0.9.7-2026.1028.09.35
//
// 与 Rust core/build.rs 的 emit_version() 完全同构：
// 同一批环境变量注入，DLL 与 APK 的版本串只差端标识。
// 缺失时退回与 Rust 侧一致的默认（snapshot + 0.0.0 + 当前时间），
// 并打 warning —— 版本写错比退回默认更危险。
fun translatorVersionName(): String {
    val channels = setOf("snapshot", "alpha", "beta", "pre", "release", "stable", "current")
    val channel = (System.getenv("TRANSLATOR_CHANNEL") ?: "").lowercase()
        .let { if (channels.contains(it)) it else "snapshot".also {
            if (System.getenv("TRANSLATOR_CHANNEL") != null)
                logger.warn("未知渠道 TRANSLATOR_CHANNEL=${System.getenv("TRANSLATOR_CHANNEL")}，退回 snapshot")
        } }

    val semverEnv = System.getenv("TRANSLATOR_SEMVER") ?: ""
    val semverOk = Regex("""^\d+\.\d+\.\d+$""").matches(semverEnv)
    val semver = if (semverOk) semverEnv else "0.0.0".also {
        logger.warn("TRANSLATOR_SEMVER='$semverEnv' 不是 X.Y.Z，退回 0.0.0")
    }

    // 时间：优先 TRANSLATOR_BUILD_TIME（"YYYY-MM-DD HH:MM:SS" 本地时间），
    // 缺失用当前时间 —— 与 build.rs 的 parse_time/utc_now 兜底逻辑对齐。
    // ⚠️ 不用 java.time.LocalDateTime —— Gradle Kotlin DSL 脚本类加载器
    // 在 JDK25 下解析其属性会报 Unresolved reference（实测踩过），
    // Calendar 是 javax 老 API，兼容性最稳（类名走顶部 import）。
    val t = parseBuildTime(System.getenv("TRANSLATOR_BUILD_TIME"))
        ?: Calendar.getInstance().let { c ->
            intArrayOf(
                c.get(Calendar.YEAR),
                c.get(Calendar.MONTH) + 1,
                c.get(Calendar.DAY_OF_MONTH),
                c.get(Calendar.HOUR_OF_DAY),
                c.get(Calendar.MINUTE),
            )
        }
    val time = "%04d.%02d%02d.%02d.%02d".format(t[0], t[1], t[2], t[3], t[4])
    return "android-$channel-v$semver-$time"
}

fun parseBuildTime(s: String?): IntArray? {
    if (s.isNullOrBlank()) return null
    val m = Regex("""^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2})""").find(s.trim()) ?: return null
    val (y, mo, d, hh, mi) = m.destructured
    val a = intArrayOf(y.toInt(), mo.toInt(), d.toInt(), hh.toInt(), mi.toInt())
    return if (a[1] in 1..12 && a[2] in 1..31 && a[3] <= 23 && a[4] <= 59) a else null
}