package com.translator.app

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.util.Log

/**
 * 发起授权时暂存的会话参数。
 *
 * ## 为什么是顶层类
 * 早期放在 companion object 里，Kotlin 访问需写
 * `ProjectionPermissionActivity.Companion.PendingParams`（companion 内的类
 * 不能用 `Outer.Inner` 简写）—— 放在顶层可直接
 * `ProjectionPermissionActivity.PendingParams`。
 */
data class PendingProjectionParams(
    val useSystemAudio: Boolean,
    val session: Long,
    /** 内放路的 Rust 采集器下标（-1 = 不起内放） */
    val sysIndex: Int,
    /** 麦克风路的 Rust 采集器下标（-1 = 不起麦克风） */
    val micIndex: Int,
)

/**
 * MediaProjection 授权请求入口。
 *
 * ## 为什么必须是独立 Activity
 *
 * `MediaProjectionManager.createScreenCaptureIntent()` 返回的 Intent
 * 必须由 Activity 发起，且授权结果通过 `onActivityResult` 返回。
 * 系统音频捕获的授权无法静默完成 —— 这是 OS 设计。
 *
 * ## 结果去向（2026-10-27 修正 Android 14 时序）
 *
 * 授权成功 → [AudioCaptureService.startWithProjectionResult]：
 * 只把 resultCode + data 经 Intent 交给服务，由服务在
 * startForeground(MEDIA_PROJECTION) **之后**创建 projection。
 * ⚠️ 绝不能在 Activity 里调 getMediaProjection() 再传递对象 ——
 * Android 14 会当场抛 SecurityException（旧实现即因此闪退）。
 * 用户拒绝 → 降级起麦克风服务。两条路径都必须把服务起起来，
 * 否则 Dart 的 startCapture 会一直等不到 activeMode 变化。
 *
 * ## 卓易通容器
 *
 * 容器内此授权是否可用**未知**（需宿主 HarmonyOS 代理，见 DESIGN §2.5）。
 * 若 createScreenCaptureIntent 直接抛异常 → 走降级。
 */
class ProjectionPermissionActivity : Activity() {

    companion object {
        private const val TAG = "TranslatorCapture"
        private const val REQUEST_CODE = 10086

        /** MainActivity 发起授权时暂存的会话参数（同进程静态传递）。 */
        @Volatile
        var pending: PendingProjectionParams? = null

        fun request(context: Context) {
            val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE)
                as MediaProjectionManager
            context.startActivity(
                Intent(context, ProjectionPermissionActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    .putExtra("pending_intent", mgr.createScreenCaptureIntent())
            )
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        // 跟随主界面的方向策略：手机锁竖屏、平板可旋转。
        // 否则横屏平板上弹投屏授权会闪成竖屏，弹完再转回来，很刺眼。
        OrientationPolicy.apply(this)
        super.onCreate(savedInstanceState)

        val p = pending
        val pendingIntent = intent.getParcelableExtra<Intent>("pending_intent")
        if (pendingIntent == null) {
            // 发起方没留下参数（异常流）—— 不能把服务卡死，按麦克风起
            p?.let {
                AudioCaptureService.start(this, false, it.session, -1, it.micIndex)
            }
            pending = null
            finish()
            return
        }
        if (p == null) {
            // 无会话参数（比如进程重建），无法推流 —— 直接退出，
            // Dart 侧经 captureStatus 看到 mode 不变会自行超时处理
            finish()
            return
        }

        try {
            startActivityForResult(pendingIntent, REQUEST_CODE)
        } catch (e: Exception) {
            // 卓易通容器等受限环境下可能直接抛异常 → 只起麦克风路
            Log.w(TAG, "投屏授权不可用：${e.message}")
            AudioCaptureService.degradeReason = "投屏授权不可用：${e.message}"
            AudioCaptureService.start(this, false, p.session, -1, p.micIndex)
            pending = null
            finish()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val p = pending
        pending = null

        if (requestCode == REQUEST_CODE && p != null) {
            if (resultCode == RESULT_OK && data != null) {
                AudioCaptureService.degradeReason = ""
                // ⚠️ 只回传 resultCode+data —— Android 14 禁止在 Activity 里
                // 先 getMediaProjection 再交给服务（当场 SecurityException，
                // 2026-10-27 实测闪退）。projection 由服务 startForeground
                // (MEDIA_PROJECTION) 成功后自行创建。双路：内放+麦克风并行。
                AudioCaptureService.startWithProjectionResult(
                    this, p.session, p.sysIndex, p.micIndex, resultCode, data
                )
            } else {
                AudioCaptureService.degradeReason = "用户未授予投屏权限，仅麦克风转写"
                Log.w(TAG, AudioCaptureService.degradeReason)
                // 拒绝投屏 → 只起麦克风路（内放路 -1 不起）
                AudioCaptureService.start(this, false, p.session, -1, p.micIndex)
            }
        }
        finish()
    }
}
