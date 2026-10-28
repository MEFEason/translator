// Translator Android 根构建脚本（Kotlin DSL）
//
// ⚠️ 此文件必须存在：Flutter 工具靠它判断工程使用 Gradle
// （`isUsingGradle` → `hostAppGradleFile`）。缺失会让 Flutter 回退到
// `android/AndroidManifest.xml` 路径，进而误判为已删除的 v1 embedding，
// 报 "Build failed due to use of deleted Android v1 embedding"。
//
// ⚠️ Gradle 9 起 `project.buildDir` 已废弃且是 `File` 类型（非 String），
// 赋字符串会报 "Assignment type mismatch"。改用 `layout.buildDirectory`。
//
// ⚠️ 这里**不要**声明 `allprojects { repositories { ... } }`：
// settings.gradle.kts 设了 `RepositoriesMode.FAIL_ON_PROJECT_REPOS`，
// 两处同时声明会报 "repository 'Google' was added by build file"。
// 仓库统一由 settings.gradle.kts 的 dependencyResolutionManagement 管理。

import org.gradle.buildconfiguration.tasks.UpdateDaemonJvm

// 构建产物统一放到 android/build 下，避免污染 Flutter 的 .dart_tool
rootProject.layout.buildDirectory.set(file("../build"))
subprojects {
    layout.buildDirectory.set(rootProject.layout.buildDirectory.dir(name))
    evaluationDependsOn(":app")
}

// ⚠️ 2026-10-27 修复：`:file_picker:checkDebugAarMetadata` 失败
//
//   "Dependency ':flutter_plugin_android_lifecycle' requires libraries and
//    applications that depend on it to compile against version 36 or later
//    of the Android APIs. :file_picker is currently compiled against android-34."
//
// 根因（实测各插件源码，非推测）：
//   - flutter_plugin_android_lifecycle 2.0.35：`compileSdk = flutter.compileSdkVersion`
//     → 解析为 36
//   - file_picker 8.3.7：**硬编码** `compileSdk 34`
//   AGP 9 校验 AAR metadata：依赖方要求 36，被依赖方只有 34 → 直接失败。
//
// 为什么统一 compileSdk 而不是升级 file_picker：
//   file_picker 8.3.7 → 13.x 是**大版本跳跃**，Dart 侧
//   `FilePicker.platform.saveFile()` 的语义与参数都变了
//   （且 Android 8.x 本就不支持 saveFile），会牵动导出功能重写。
//   统一 compileSdk 是 Flutter 官方模板的做法，不动任何插件源码。
subprojects {
    // ⚠️ 2026-10-27 修复：统一所有插件子工程的 compileSdk = 36。
    //
    // 背景：flutter_plugin_android_lifecycle 2.0.35 用
    // `compileSdk = flutter.compileSdkVersion`（=36），而 file_picker 8.3.7
    // **硬编码** `compileSdk 34` → AGP 校验 AAR metadata 时报
    //   "':flutter_plugin_android_lifecycle' requires ... compile against
    //    version 36 or later ... ':file_picker' is currently compiled
    //    against android-34"
    //
    // 为什么不用 `plugins.withId` 直接设：插件自己的 build.gradle
    // （file_picker 硬写 34）在本块**之后**才评估，会把我们的值覆盖回去
    // → 必须在插件评估完成后再改写。
    //
    // 为什么不用无条件的 `afterEvaluate`：`:app` 被 `evaluationDependsOn`
    // 提前评估过，此时调用会报
    //   "Cannot run Project.afterEvaluate(Action) when the project is already evaluated."
    // → 故先判断 state，必要时才注册回调。
    //
    // 为什么用 CommonExtension：AGP 9 的 DSL 类换包了。
    //   com.android.build.gradle.LibraryExtension      → deprecated
    //   com.android.build.gradle.ApplicationExtension   → 已移除
    //   com.android.build.api.dsl.CommonExtension        → 正确，且无泛型参数
    fun forceCompileSdk() {
        extensions.findByType<com.android.build.api.dsl.CommonExtension>()
            ?.compileSdk = 36
    }
    if (state.executed) {
        forceCompileSdk()
    } else {
        afterEvaluate { forceCompileSdk() }
    }
}

tasks.register<Delete>("clean") {
    delete(rootProject.layout.buildDirectory)
}

// ⚠️ 2026-10-27 修复：Gradle 9.3 的 `updateDaemonJvm` 任务构建失败
//   "Execution failed for task ':updateDaemonJvm'
//    > Error while evaluating property 'toolchainDownloadUrls'
//       > Toolchain download repositories have not been configured."
//
// 根因链（逐环实测，非推测）：
//   1. IDE（Android Studio）给 Gradle 传了 daemon JVM 条件：
//      `--jvm-version 25 --jvm-vendor jetbrains`
//   2. 该任务要为**多个平台**（x86_64/aarch64 × Linux/MacOS/Windows…）
//      生成 JDK 下载 URL，才能写出 gradle/daemon-jvm.properties。
//   3. 本机没有配 toolchain 下载仓库（foojay resolver）→ 任务直接失败。
//
// 为什么不配 foojay 而是本地满足：
//   - 本机**已有**满足条件的 JDK：Android Studio 自带 JBR 就是
//     JetBrains 厂商的 JDK 25（`java.vendor = JetBrains s.r.o.`），
//     `./gradlew -q javaToolchains` 能列出来。
//   - foojay-resolver-convention 0.9.0 与 Gradle 9.3.1 **不兼容**
//     （引用了 9.4+ 才有的 `JvmVendorSpec.IBM_SEMERU`），一加就崩：
//     "NoSuchFieldError: JvmVendorSpec does not have member field IBM_SEMERU"。
//   - 本工程**零网络依赖**是产品硬约束，不该引入运行时下载 JDK 的路径。
//
// 官方推荐做法（Gradle 文档 gradle_daemon.html）：
//   "If you want to disable the generation of URLs by the updateDaemonJvm task:
//      tasks.named("updateDaemonJvm") { toolchainDownloadUrls.empty() }"
// 空 URL map ⇒ 不需要下载仓库 ⇒ 任务通过。
tasks.named<UpdateDaemonJvm>("updateDaemonJvm") {
    toolchainDownloadUrls.empty()
}