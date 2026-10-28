package com.translator.app

import android.content.Context
import android.util.Log
import java.io.File
import java.io.FileOutputStream

/**
 * 首启模型解压器（2026-10-28 模型内置化，任务 #16）。
 *
 * ## 为什么模型进 APK assets 再解压，而不是直接读 assets
 *
 * Rust 侧（sherpa-onnx / ORT）按**文件路径**加载模型，assets 是
 * APK 内嵌流、没有真实路径 —— 必须物化到文件系统。落点选
 * **内部私有目录** `filesDir/models`（属主=应用 UID，权限天然正确；
 * 外部目录有 adb push 属主坑的历史教训）。
 *
 * ## 版本标记避免重复解压
 *
 * 标记文件 `filesDir/models/.extracted` 内容 = 本次 APK 的 versionName。
 * - 标记缺失 / 内容不等（升级换模型）→ 清旧目录重解
 * - 标记相等 → 秒过
 *
 * ⚠️ 标记**最后写**：解压中途被杀（安装器冲突、低电）时目录是残缺的，
 * 若标记先写，下次启动会误判"已解压"→ 加载半截模型必崩。
 * 宁可多解一次，不可少解一次。
 *
 * ## 进度回调
 *
 * Dart 侧 bootstrap 要等解压完成才探测模型（任务 #17），
 * 441MB 解压约 5~15 秒，期间 UI 显示进度 —— 通过
 * [progressChannel] 回推（MainActivity 注册的 EventChannel/MethodChannel）。
 */
object ModelExtractor {

    private const val TAG = "TranslatorModels"
    private const val MARKER = ".extracted"

    @Volatile
    private var extracting = false

    /** 进度回调（0f~1f，阶段描述）。由 MainActivity 注入，转给 Dart。 */
    @Volatile
    var onProgress: ((Float, String) -> Unit)? = null

    /**
     * 确保模型已就位（幂等，可在任意线程调用）。
     *
     * @return 就绪后的模型目录绝对路径；失败返回 null（UI 提示后
     *         可退回 mock 后端，与会话创建的容错策略一致）。
     */
    fun ensureModels(context: Context): String? {
        val modelsDir = File(context.filesDir, "models")
        val marker = File(modelsDir, MARKER)
        // versionName 含端标识与时间戳（android-beta-v0.9.7-2026.1028.09.35），
        // 任何一次重新打包都会变 → 天然失效旧解压结果。
        // 注意：assets 与 APK 同生命周期，versionName 变 = assets 内容必然已变。
        val version = try {
            context.packageManager.getPackageInfo(context.packageName, 0)
                .versionName ?: "unknown"
        } catch (e: Exception) {
            "unknown"
        }

        synchronized(this) {
            if (marker.exists() && marker.readText().trim() == version) {
                Log.i(TAG, "模型已解压（$version），跳过")
                return modelsDir.absolutePath
            }
            if (extracting) {
                Log.w(TAG, "解压已在进行中，等待由 Dart 侧轮询")
                return null
            }
            extracting = true
        }

        return try {
            doExtract(context, modelsDir, version)
            modelsDir.absolutePath
        } catch (e: Exception) {
            Log.e(TAG, "模型解压失败", e)
            // 失败时清掉半成品，下次启动重试
            runCatching { modelsDir.deleteRecursively() }
            null
        } finally {
            synchronized(this) { extracting = false }
        }
    }

    private fun doExtract(context: Context, dest: File, version: String) {
        Log.i(TAG, "开始解压模型 assets/models → ${dest.absolutePath}")
        val t0 = System.currentTimeMillis()

        // 升级路径：旧解压存在但版本不等 → 先整体删除
        if (dest.exists()) dest.deleteRecursively()
        dest.mkdirs()

        val paths = context.assets.list("models")?.toList() ?: emptyList()
        if (paths.isEmpty()) {
            throw IllegalStateException("APK assets 里没有 models —— syncModels 任务未生效？")
        }

        // 先数总文件数与总字节（进度分母）。assets 拿不到单文件大小，
        // 用 list 递归计数即可（按文件数算进度，粒度足够）。
        val allFiles = mutableListOf<String>()
        fun walk(rel: String) {
            val entries = context.assets.list(rel) ?: return
            if (entries.isEmpty()) {
                allFiles.add(rel)   // 叶子 = 文件
                return
            }
            for (e in entries) walk("$rel/$e")
        }
        for (p in paths) walk("models/$p")

        var done = 0
        val total = allFiles.size.coerceAtLeast(1)
        for (f in allFiles) {
            // f 形如 "models/asr/xxx/encoder.onnx"；dest 即 ".../files/models"
            copyAsset(context, f, File(dest, f.removePrefix("models/")))
            done++
            onProgress?.invoke(
                done.toFloat() / total,
                "正在安装模型（$done/$total）"
            )
        }

        // ★ 标记最后写（见类注释）
        File(dest, MARKER).writeText(version)
        Log.i(TAG, "模型解压完成：${allFiles.size} 个文件，" +
            "耗时 ${(System.currentTimeMillis() - t0) / 1000}s")
        onProgress?.invoke(1f, "模型安装完成")
    }

    private fun copyAsset(context: Context, assetPath: String, out: File) {
        out.parentFile?.mkdirs()
        context.assets.open(assetPath).use { input ->
            FileOutputStream(out).use { output ->
                val buf = ByteArray(128 * 1024)   // 128KB：大文件（onnx）拷贝吞吐足够
                while (true) {
                    val n = input.read(buf)
                    if (n <= 0) break
                    output.write(buf, 0, n)
                }
                output.flush()
            }
        }
    }

    /** 是否正在解压（Dart 轮询用）。 */
    fun isExtracting(): Boolean = extracting
}
