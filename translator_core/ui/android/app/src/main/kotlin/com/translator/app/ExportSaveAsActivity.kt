package com.translator.app

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.util.Log

/**
 * 转写导出：打开系统「保存文件」对话框并写入内容。
 *
 * ## 为什么不用 file_picker
 *
 * `file_picker` 8.x 的 `saveFile()` 在 Android 上**不支持**
 * （抛 UnimplementedError / 返回 null），这正是旧实现把导出
 * 退化成「写应用私有目录」的原因 —— 用户在文件管理器里根本看不到。
 *
 * ## 正确做法：Storage Access Framework（SAF）
 *
 * `ACTION_CREATE_DOCUMENT` 让**系统**弹「另存为」对话框：
 * 用户选目录 + 填文件名 → 系统返回一个可写 URI。
 * 这是 Android 上唯一能写到用户指定位置（Downloads / 微信收藏 / 百度网盘…）
 * 的正规途径，且**不需要任何存储权限**（SAF 自带授权）。
 *
 * ## 流程
 *
 * 1. Dart 调 `exportSaveAs{content, fileName}`
 * 2. 本 Activity 启动 `ACTION_CREATE_DOCUMENT`（类型 text/plain）
 * 3. `onActivityResult` 拿到 URI → `contentResolver.openOutputStream` 写入
 * 4. 回 Dart：成功给 URI 字符串，失败/取消给 null
 *
 * ## 结果传递
 *
 * 同进程静态暂存（同 [ProjectionPermissionActivity] 的做法）：
 * `pendingContent` 存待写内容，`resultUri` 存结果。
 */
class ExportSaveAsActivity : Activity() {

    companion object {
        private const val TAG = "TranslatorExport"
        private const val REQUEST_CODE = 10087

        /** 发起方（Dart 侧经 MainActivity 转发）暂存的待写内容与文件名。 */
        @Volatile
        var pendingContent: String? = null

        @Volatile
        var pendingFileName: String = ""

        /** 完成回调：成功给 URI 字符串，取消/失败给 null。 */
        @Volatile
        var onDone: ((String?) -> Unit)? = null

        /**
         * 发起「另存为」。
         *
         * @param content 待写入的文本内容
         * @param fileName 建议文件名（系统对话框里可改）
         * @param callback 完成回调（成功 = URI 字符串，取消 = null）
         */
        fun start(activity: Activity, content: String, fileName: String, callback: (String?) -> Unit) {
            pendingContent = content
            pendingFileName = fileName
            onDone = callback
            val intent = Intent(activity, ExportSaveAsActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            activity.startActivity(intent)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val content = pendingContent
        if (content == null) {
            Log.w(TAG, "无待写内容，放弃导出")
            finishWith(null)
            return
        }

        try {
            val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "text/plain"
                putExtra(Intent.EXTRA_TITLE, pendingFileName)
            }
            startActivityForResult(intent, REQUEST_CODE)
        } catch (e: Exception) {
            Log.e(TAG, "无法发起另存为：${e.message}", e)
            finishWith(null)
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_CODE) {
            finish()
            return
        }
        if (resultCode != RESULT_OK || data?.data == null) {
            Log.i(TAG, "用户取消另存为")
            finishWith(null)
            return
        }
        val uri = data.data!!
        val content = pendingContent ?: ""
        try {
            contentResolver.openOutputStream(uri, "wt")?.use { out ->
                // ★ 先写 UTF-8 BOM（EF BB BF），再写正文
                //（2026-10-29 用户实测：导出的 txt 在手机上打开乱码）。
                //
                // 为什么：正文是 UTF-8，但**无 BOM 的 UTF-8 纯文本**在
                // 中文环境的阅读器里会被按 GBK 猜（WPS、系统阅读器、
                // 微信文件预览都有这行为）—— 这是手机「乱码」的根因，
                // 不是写坏了编码。加 BOM 后这些阅读器都能正确识别 UTF-8。
                //
                // ⚠️ 只在 Android 加：Windows 端记事本/VSCode 对无 BOM
                // UTF-8 识别良好，且 Windows 的 txt 关联可能被老工具
                // 处理 BOM 时多吐一个可见字符，没必要跟着改。
                out.write(byteArrayOf(0xEF.toByte(), 0xBB.toByte(), 0xBF.toByte()))
                out.write(content.toByteArray(Charsets.UTF_8))
                out.flush()
            } ?: throw IllegalStateException("openOutputStream 返回 null")
            Log.i(TAG, "导出成功：$uri")
            finishWith(uri.toString())
        } catch (e: Exception) {
            Log.e(TAG, "写入失败：${e.message}", e)
            finishWith(null)
        }
    }

    /** 回传结果并关闭本 Activity。 */
    private fun finishWith(uri: String?) {
        val cb = onDone
        onDone = null
        pendingContent = null
        // 回到 UI 线程再回调（Dart 侧 Channel 调用要求主线程）
        runOnUiThread {
            cb?.invoke(uri)
            finish()
        }
    }
}
