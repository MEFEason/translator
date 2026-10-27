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