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
    val sourceIndex: Int,
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
 * ## 结果去向（2026-10-26 接线改造）
 *
 * 授权成功 → 直接调 [AudioCaptureService.startWithProjection]
 * （同进程，projection 经静态暂存交给服务的 startForeground 时序）；
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
                AudioCaptureService.start(this, false, it.session, it.sourceIndex)
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
            // 卓易通容器等受限环境下可能直接抛异常 → 走降级
            Log.w(TAG, "投屏授权不可用：${e.message}")
            AudioCaptureService.degradeReason = "投屏授权不可用：${e.message}"
            AudioCaptureService.start(this, false, p.session, p.sourceIndex)
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
                AudioCaptureService.startWithProjection(
                    this, p.session, p.sourceIndex, resultCode, data
                )
            } else {
                AudioCaptureService.degradeReason = "用户未授予投屏权限，已降级为麦克风"
                Log.w(TAG, AudioCaptureService.degradeReason)
                AudioCaptureService.start(this, false, p.session, p.sourceIndex)
            }
        }
        finish()
    }
}
