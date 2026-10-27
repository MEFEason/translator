package com.translator.app

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import kotlin.concurrent.thread

/**
 * 应用入口 Activity + 采集编排通道。
 *
 * ## 为什么必须单独一个类
 *
 * Flutter **已删除 Android v1 embedding**（Flutter 3.x 起）。
 * 若 Manifest 直接声明 `io.flutter.embedding.android.FlutterActivity`
 * 且不带 meta-data，构建会报 "deleted Android v1 embedding"。
 *
 * ## 采集链路（2026-10-26 接线）
 *
 * 会话与 Rust 句柄的**所有权在 Dart**（DynamicLibrary.open 同一 .so），
 * Kotlin 只是生产者：
 *
 *   Dart session_start 成功
 *     → MethodChannel startCapture{useSystemAudio, session, sourceIndex}
 *       →（内放）先弹 ProjectionPermissionActivity 拿授权
 *       → AudioCaptureService 前台服务（同进程 —— 跨进程指针无效，
 *         见 Manifest 注释）
 *     → Kotlin AudioRecord 320 样本/块 → sessionPushChunk → Rust 队列
 *
 *   Dart stop：**先** stopCapture（等生产者线程真停）→ 再 session_free，
 *   顺序反了会踩已释放的会话指针。
 *
 * ## 权限
 *
 * - RECORD_AUDIO：麦克风与 FGS-MICROPHONE 类型（Android 14 未授权会抛
 *   SecurityException）都硬依赖，这里运行时申请。
 * - POST_NOTIFICATIONS：33+ 前台服务通知可见性。
 * - MediaProjection：OS 设计不可持久化，每次冷启动都要重新弹（每次
 *   startCapture(useSystemAudio) 都会走 ProjectionPermissionActivity）。
 */
class MainActivity : FlutterActivity() {

    companion object {
        private const val TAG = "TranslatorCapture"
        const val CHANNEL = "com.translator.app/capture"
        private const val REQ_PERMS = 20261
    }

    private var channel: MethodChannel? = null

    /** 权限申请期间挂起的 startCapture 参数，授权回来后继续。 */
    private class PendingStart(
        val useSystemAudio: Boolean,
        val session: Long,
        val sourceIndex: Int,
        val reply: MethodChannel.Result,
    )

    private var pending: PendingStart? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        // ⚠️ 必须在 super.onCreate 之前定方向：否则首帧会按系统当前方向
        //    渲染出来，手机横屏用户会看到「转一下才立正」的闪动。
        OrientationPolicy.apply(this)
        super.onCreate(savedInstanceState)
    }

    /**
     * 配置变化（旋转 / 折叠屏展开 / 分屏）。
     *
     * Manifest 的 `configChanges` 含 `orientation|screenSize|smallestScreenSize`
     * → Activity **不重建**，Flutter 侧只收到尺寸变化并重排。
     * 这点很重要：重建 Activity 会丢 Rust 会话指针，正在进行的转写全断。
     *
     * 这里重新判定方向策略 —— 折叠屏从折叠态（按手机锁竖屏）展开成
     * 大屏时，要立刻切成「可旋转」。
     */
    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        OrientationPolicy.apply(this)
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        channel = MethodChannel(flutterEngine.dartExecutor.binaryMessenger, CHANNEL)
            .also { it.setMethodCallHandler(this::onMethodCall) }
    }

    private fun onMethodCall(call: MethodCall, reply: MethodChannel.Result) {
        when (call.method) {
            "startCapture" -> startCapture(call, reply)
            "stopCapture" -> stopCapture(reply)
            "captureStatus" -> reply.success(
                mapOf(
                    "mode" to AudioCaptureService.activeMode,
                    "degrade" to AudioCaptureService.degradeReason,
                    "isZhuoYiTong" to AudioCaptureService.isZhuoYiTong,
                )
            )
            "externalFilesDir" ->
                reply.success(getExternalFilesDir(null)?.absolutePath ?: filesDir.absolutePath)
            "deviceProfile" -> reply.success(OrientationPolicy.profileOf(this))
            "setCaptureGranted" -> {
                // Kotlin 是授权的实际感知方 → 把结果转给 Rust 会话，
                // 让 capture_capability 如实上报（禁止静默降级，DESIGN §2.5.2）
                val session = call.argument<Long>("session") ?: 0L
                val granted = call.argument<Boolean>("granted") ?: false
                reply.success(NativeBridge.sessionSetCaptureGranted(session, granted))
            }
            else -> reply.notImplemented()
        }
    }

    // ── startCapture ─────────────────────────────────

    private fun startCapture(call: MethodCall, reply: MethodChannel.Result) {
        val useSystemAudio = call.argument<Boolean>("useSystemAudio") ?: false
        val session = (call.argument<Number>("session"))?.toLong() ?: 0L
        val sourceIndex = (call.argument<Number>("sourceIndex"))?.toInt() ?: 0

        if (session == 0L) {
            reply.error("BAD_SESSION", "会话指针为空：Dart 须先 session_start", null)
            return
        }

        // 麦克风必需权限缺 → 先申请，授权回调里续跑（系统内放同样要
        // RECORD_AUDIO 之外的投屏授权，见下）
        //
        // ⚠️ 此处**不回包**：reply 挂起，等 onRequestPermissionsResult
        // 里 launchCapture(...) 才回 —— Dart 侧 startCapture 的 await
        // 因此会等到用户完成授权弹窗，这是预期行为。
        if (!hasAudioPermission()) {
            pending = PendingStart(useSystemAudio, session, sourceIndex, reply)
            requestPermissions(audioPermList(), REQ_PERMS)
            return
        }

        launchCapture(useSystemAudio, session, sourceIndex, reply)
    }

    /** 权限齐备后的真正启动分流。 */
    private fun launchCapture(
        useSystemAudio: Boolean,
        session: Long,
        sourceIndex: Int,
        reply: MethodChannel.Result,
    ) {
        if (useSystemAudio && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            // 内放：MediaProjection 授权必须由 Activity 前台弹窗发起。
            // 暂存参数，授权结果在 ProjectionPermissionActivity 处理。
            ProjectionPermissionActivity.pending =
                PendingProjectionParams(useSystemAudio, session, sourceIndex)
            try {
                ProjectionPermissionActivity.request(this)
            } catch (e: Exception) {
                // 容器等受限环境连 createScreenCaptureIntent 都可能抛
                // → 降级麦克风，**必须回包**（否则 Dart 的 await 挂死）
                Log.w(TAG, "投屏授权流无法发起：${e.message}，降级麦克风")
                AudioCaptureService.degradeReason = "投屏授权不可用：${e.message}"
                ProjectionPermissionActivity.pending = null
                AudioCaptureService.start(this, false, session, sourceIndex)
                reply.success(0)
                return
            }
            // 授权结果异步：这里先回「已受理」。Dart 侧随后经
            // captureStatus 轮询 activeMode 决定继续/降级。
            reply.success(0)
        } else {
            AudioCaptureService.start(this, false, session, sourceIndex)
            reply.success(0)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQ_PERMS) return
        val p = pending ?: return
        pending = null
        val granted = grantResults.isNotEmpty()
            && grantResults[0] == PackageManager.PERMISSION_GRANTED
        if (!granted) {
            // 麦克风都没有 → 转达给 Dart（UI 弹引导），不得静默起服务
            Log.w(TAG, "RECORD_AUDIO 被拒，无法采集")
            p.reply.error("PERMISSION_DENIED", "麦克风权限未授予", null)
            return
        }
        runOnUiThread { launchCapture(p.useSystemAudio, p.session, p.sourceIndex, p.reply) }
    }

    /** pending 路径下 reply 延后给 —— 用空实现占位，真实回包在续跑处。 */
    private fun replySuccessLater() {
        // Dart 侧 startCapture 是 await：权限弹窗期间它只是多等一个
        // 用户交互。结果在 onRequestPermissionsResult → launchCapture
        // 的 reply 里给出，不需要在这里回包。
    }

    private fun hasAudioPermission(): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            checkSelfPermission(Manifest.permission.RECORD_AUDIO) ==
                PackageManager.PERMISSION_GRANTED
        } else true

    private fun audioPermList(): Array<String> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            // 前台服务通知（33+ 不给就看不见"正在转写"通知）
            arrayOf(Manifest.permission.RECORD_AUDIO, Manifest.permission.POST_NOTIFICATIONS)
        } else {
            arrayOf(Manifest.permission.RECORD_AUDIO)
        }

    // ── stopCapture ──────────────────────────────────

    /**
     * 停止采集。⚠️ 同步语义：回包前**必须等采集线程真退出** ——
     * Dart 拿到回包后就会 session_free，顺序错了就是 use-after-free。
     */
    private fun stopCapture(reply: MethodChannel.Result) {
        thread(name = "capture-stop") {
            try {
                AudioCaptureService.instance?.stopAndAwait()
            } catch (e: Exception) {
                Log.w(TAG, "停止采集异常（仍回包）", e)
            }
            Handler(Looper.getMainLooper()).post { reply.success(0) }
        }
    }

    // ── 投屏授权结果分发 ─────────────────────────────

    /** 由 [ProjectionPermissionActivity] 在结果就绪后回调。 */
    fun onProjectionResult(
        resultCode: Int,
        data: Intent?,
        useSystemAudio: Boolean,
        session: Long,
        sourceIndex: Int,
    ) {
        if (resultCode == RESULT_OK && data != null) {
            AudioCaptureService.startWithProjection(this, session, sourceIndex, resultCode, data)
        } else {
            // 用户拒绝投屏 → 按麦克风继续（Rust 侧 capability 如实标降级）
            Log.w(TAG, "投屏未授权，降级麦克风")
            AudioCaptureService.start(this, false, session, sourceIndex)
        }
    }

    /** 授权流被容器等环境直接掐断时的兜底（不卡死 Dart 的等待）。 */
    fun onProjectionFlowAbandoned(session: Long, sourceIndex: Int) {
        AudioCaptureService.start(this, false, session, sourceIndex)
    }

    override fun onDestroy() {
        channel?.setMethodCallHandler(null)
        channel = null
        super.onDestroy()
    }
}
