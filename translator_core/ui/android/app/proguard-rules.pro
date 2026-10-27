# Translator Android release 混淆规则
#
# ## 为什么需要这个文件
# build.gradle.kts 的 release 块引用了 "proguard-rules.pro"，
# 但文件缺失 → release 构建直接失败（AGP 报找不到文件）。
#
# ## 保留规则
# 1. JNI 类：NativeBridge 的 external fun 由 JNI 按名字解析
#    （Java_com_translator_app_NativeBridge_nativeXxx），
#    混淆类名/方法名会导致 UnsatisfiedLinkError。
# 2. Kotlin 的 @Volatile 静态字段（AudioCaptureService.activeMode 等）
#    被 Dart 经反射/MethodChannel 读取 —— 保留字段名。
# 3. Flutter 插件的 Activity/Service/Receiver 由 Manifest 引用，
#    混淆会破坏实例化（Manifest 里的名字与运行时不一致）。

# ── JNI 桥接 ────────────────────────────────────────
-keep class com.translator.app.NativeBridge { *; }
-keepclasseswithmembernames class com.translator.app.NativeBridge {
    native <methods>;
}

# ── 采集服务与入口 ──────────────────────────────────
-keep class com.translator.app.AudioCaptureService { *; }
-keep class com.translator.app.MainActivity { *; }
-keep class com.translator.app.ProjectionPermissionActivity { *; }

# ── Flutter 引擎要求 ────────────────────────────────
-keep class io.flutter.** { *; }
-keep class io.flutter.plugins.** { *; }
-dontwarn io.flutter.embedding.**

# ── Kotlin 协程/标准库 ─────────────────────────────
-dontwarn kotlinx.coroutines.**
