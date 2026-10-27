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

// 构建产物统一放到 android/build 下，避免污染 Flutter 的 .dart_tool
rootProject.layout.buildDirectory.set(file("../build"))

subprojects {
    layout.buildDirectory.set(rootProject.layout.buildDirectory.dir(name))
    evaluationDependsOn(":app")
}

tasks.register<Delete>("clean") {
    delete(rootProject.layout.buildDirectory)
}