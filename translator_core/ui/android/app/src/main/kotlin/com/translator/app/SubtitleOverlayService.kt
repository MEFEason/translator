package com.translator.app

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.PixelFormat
import android.net.Uri
import android.os.Build
import android.os.IBinder
import android.provider.Settings
import android.view.Choreographer
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.WindowManager
import android.widget.LinearLayout

/**
 * 悬浮字幕窗（Android 端，2026-10-28 任务 #24）。
 *
 * ## 为什么必须原生 Overlay，而不是桌面那套「第二实例」
 *
 * 桌面端字幕窗 = 同一 exe 带 TRANSLATOR_LYRICS=1 再起一个进程
 * （Flutter 多实例）。Android **禁止应用自spawn进程**（无 Process.start
 * 语义），第二实例方案整体不可用。Android 的悬浮窗只有一条路：
 * WindowManager + TYPE_APPLICATION_OVERLAY（系统级悬浮层），
 * 由一个前台服务持有 —— 与采集服务同款生命周期约束。
 *
 * ## 数据流
 *
 *   Dart（_poll 字幕更新时）
 *     → MethodChannel updateSubtitle {lines:[{s,t}], partial}
 *       → 本服务把 TextView 重排上屏
 *
 * 文本渲染走原生 View 而非 Flutter —— Overlay 里跑第二个 Flutter
 * View 需要 FlutterView 独立渲染表面，复杂度陡增且与主 Flutter
 * 引擎抢资源；字幕只是几行文字，TextView 足够且省电。
 *
 * ## 权限（Android 6+ 特殊授权）
 *
 * SYSTEM_ALERT_WINDOW 不走运行时弹窗，必须跳系统设置页
 * ACTION_MANAGE_OVERLAY_PERMISSION 由用户手动授予。
 * [hasPermission] / [requestPermission] 封装这两步。
 *
 * ## 交互（2026-10-28 用户定版：两行、满宽）
 *
 * - 尺寸：**宽度吃满屏幕**，高度恰好两行 —— 第 1 行英文（原文），
 *   第 2 行中文（译文）。固定两个 TextView 只换 text，
 *   不整表重建（避免每帧 removeAllViews 的闪烁与 GC 压力）。
 * - 拖动：满宽窗无横向移动意义 → 只允许**垂直拖动**调高度位置
 *   （gravity=TOP，y 向下为正，手指方向即窗口方向）。
 * - 默认贴屏幕底部（初始 y = 屏高 - 预留两行高度），
 *   半透明深底、白字 —— 会议场景遮在视频窗口下沿不挡脸。
 * - 实时性：partial（流式识别中）非空时顶替英文行实时刷新，
 *   中文行保持最近一次已定译文 —— 与主界面「EN 先上屏、译文后补」
 *   同一策略。
 */
class SubtitleOverlayService : Service() {

    companion object {
        private const val TAG = "TranslatorSubtitle"
        private const val CHANNEL_ID = "translator_subtitle"
        private const val NOTIFICATION_ID = 1002

        @Volatile
        var instance: SubtitleOverlayService? = null

        fun hasPermission(context: Context): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M)
                Settings.canDrawOverlays(context)
            else true

        /** 跳系统设置页请求悬浮窗授权（用户手动开）。 */
        fun requestPermission(context: Context) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                context.startActivity(
                    Intent(
                        Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
                        Uri.parse("package:${context.packageName}")
                    ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                )
            }
        }

        fun start(context: Context) {
            val i = Intent(context, SubtitleOverlayService::class.java)
            context.startForegroundService(i)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, SubtitleOverlayService::class.java))
        }
    }

    private var windowManager: WindowManager? = null
    private var root: LinearLayout? = null
    private var params: WindowManager.LayoutParams? = null
    private var enView: ScrollingTextLine? = null
    private var zhView: ScrollingTextLine? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
        // Android 14（API 34）起 startForeground 必须带类型，
        // 否则 MissingForegroundServiceTypeException 直接闪退（实测）。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                buildNotification(),
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIFICATION_ID, buildNotification())
        }
        if (!hasPermission(this)) {
            // Dart 侧调用前已检查，这里是竞态兜底（用户在设置页撤销授权）
            stopSelf()
            return
        }
        buildOverlay()
    }

    @SuppressLint("ClickableViewAccessibility")
    private fun buildOverlay() {
        windowManager = getSystemService(WINDOW_SERVICE) as WindowManager

        val en = textView(17f, 0xE6FFFFFF.toInt())   // 英文行（原文）
        val zh = textView(19f, Color.WHITE)          // 中文行（译文，略大更醒目）
        enView = en
        zhView = zh

        val container = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xB3000000.toInt())   // 70% 黑底
            setPadding(dp(14), dp(8), dp(14), dp(8))
            addView(en)
            addView(zh)
        }
        root = container

        // 满宽（用户 2026-10-28 定版）+ 两行高（WRAP_CONTENT）。
        // gravity=TOP：y 向下为正，初始贴底 = 屏高 - 预估值。
        val dm = resources.displayMetrics
        params = WindowManager.LayoutParams(
            WindowManager.LayoutParams.MATCH_PARENT,
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            // NOT_FOCUSABLE：不抢会议应用焦点（字幕窗绝不能吞键盘输入）
            // LAYOUT_IN_SCREEN：贴整屏坐标系
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL,
            PixelFormat.TRANSLUCENT
        ).apply {
            gravity = Gravity.TOP or Gravity.START
            x = 0
            y = dm.heightPixels - dp(140)   // 粗估两行高+边距，addView 后按实测再收
        }

        // 垂直拖动（满宽窗没有横向移动的意义）。gravity=TOP 时
        // y 向下为正，rawY 向下为正 —— 手指方向即窗口方向，不取反。
        var lastY = 0f
        container.setOnTouchListener { _, e ->
            when (e.action) {
                MotionEvent.ACTION_DOWN -> lastY = e.rawY
                MotionEvent.ACTION_MOVE -> {
                    params?.let { p ->
                        p.y += (e.rawY - lastY).toInt()
                        lastY = e.rawY
                        runCatching { windowManager?.updateViewLayout(container, p) }
                    }
                }
            }
            true
        }

        runCatching { windowManager?.addView(container, params) }
            .onFailure { android.util.Log.e(TAG, "addView 失败", it); stopSelf() }
    }

    /**
     * 更新字幕内容（用户定版：第 1 行原文、第 2 行译文，固定两行）。
     *
     * 只换 text 不重建 View —— 每帧 removeAllViews 会闪烁且制造 GC 压力。
     * partialSrc / partialZh（流式识别中/流式翻译中）非空时分别顶替
     * 对应行，与主界面「EN 先上屏、译文后补」同一实时策略。
     */
    fun updateSubtitle(
        lines: List<Pair<String, String>>,
        partialSrc: String,
        partialZh: String,
    ) {
        val en = enView ?: return
        val zh = zhView ?: return
        val last = lines.lastOrNull()
        en.post {
            // streaming 标记：partial 非空 = 正在流式识别/翻译，
            // 此时滚动位置要**跟随最新内容**（新字立刻可见），
            // 而不是每次更新都弹回开头。
            val enText = if (partialSrc.isNotEmpty()) partialSrc else (last?.first ?: "")
            val zhText = if (partialZh.isNotEmpty()) partialZh else (last?.second ?: "")
            en.setText(enText, streaming = partialSrc.isNotEmpty())
            zh.setText(zhText, streaming = partialZh.isNotEmpty())
        }
    }

    private fun buildNotification(): Notification =
        Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Translator 字幕")
            .setContentText("悬浮字幕显示中")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setOngoing(true)
            .build()

    private fun textView(sizeSp: Float, color: Int): ScrollingTextLine =
        ScrollingTextLine(this, sizeSp, color)

    private fun dp(v: Int): Int =
        (v * resources.displayMetrics.density).toInt()

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID, "悬浮字幕", NotificationManager.IMPORTANCE_MIN
            ).apply { setShowBadge(false) }
            getSystemService(NotificationManager::class.java).createNotificationChannel(ch)
        }
    }

    override fun onDestroy() {
        root?.let { runCatching { windowManager?.removeView(it) } }
        root = null
        enView?.stopScroll()
        zhView?.stopScroll()
        enView = null
        zhView = null
        instance = null
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}

/**
 * 单行横向滚动文本（字幕窗的一行）。
 *
 * ## 为什么自己画而不用系统 marquee
 *
 * `TextView.ellipsize = marquee` 有三个问题：
 * 1. 需要 `isSelected = true` 才滚动，在 Overlay 里不可靠；
 * 2. 滚动速度固定且不可调，会议场景要能读得清；
 * 3. **文本被替换时滚动位置会重置到开头** —— 而字幕是**流式**的
 *    （partial 不断变长），每次识别更新都把内容弹回开头，
 *    用户永远看不到「刚说出口」的那几个字。
 *
 * ## 本实现的滚动语义（2026-10-28 用户：实时滚动字幕和翻译内容）
 *
 * - 文本**未超出**可视宽度 → 静止贴左，不滚。
 * - 文本**超出**可视宽度 → 从左向右匀速滚动，末尾停顿一会儿，
 *   再滚回开头，循环（marquee 语义，但速度可调）。
 * - **流式更新时滚动位置跟随最新内容**：文本变长时保持
 *   「右端对齐」直到超出部分需要滚动，保证新字立刻可见。
 * - 文本变短（切到下一句）→ 立即回到起点。
 *
 * ## 实现要点
 *
 * - 滚动用 `Choreographer` 帧回调（不是 ValueAnimator）：
 *   帧率跟随屏幕，且能在 View 不可见时自动暂停。
 * - 绘制用 `Canvas.drawText` + `translate`，比 `setTranslationX`
 *   更省（不触发重布局）。
 */
private class ScrollingTextLine(
    context: Context,
    private val sizeSp: Float,
    color: Int,
) : View(context) {

    private val paint = android.graphics.Paint(
        android.graphics.Paint.ANTI_ALIAS_FLAG or android.graphics.Paint.LINEAR_TEXT_FLAG,
    ).apply {
        textSize = sizeSp * resources.displayMetrics.scaledDensity
        this.color = color
    }

    private var text: String = ""
    /** 文本总宽（px） */
    private var textWidth: Float = 0f
    /** 当前滚动偏移（px，0 = 贴左） */
    private var offset: Float = 0f
    /** 上一次帧的时间戳（ns） */
    private var lastFrameNs: Long = 0L
    private var running = false

    /**
     * 每秒滚动的像素数。
     *
     * 40dp/s 左右：中文单字约 19dp，1.5 秒能滚过一个字 ——
     * 慢到能读完，快到不磨人。字幕每秒约 5~8 字，这个速度刚好跟得上。
     */
    private val speedPxPerSec: Float =
        40f * resources.displayMetrics.density

    /** 滚到末尾后的停顿（ms）—— 给读者读完最后几个字的时间。 */
    private val endPauseMs: Long = 1200L

    private val frameCallback = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (!running) return
            val dt = if (lastFrameNs == 0L) 0L else (frameTimeNanos - lastFrameNs) / 1_000_000L
            lastFrameNs = frameTimeNanos
            advance(dt)
            invalidate()
            Choreographer.getInstance().postFrameCallback(this)
        }
    }

    /**
     * 设置文本。
     *
     * @param streaming true = 流式识别中（partial），滚动位置应**跟随最新内容**，
     *                   避免每次更新都把内容弹回开头。
     */
    fun setText(value: String, streaming: Boolean) {
        if (value == text) return
        val oldWidth = textWidth
        text = value
        textWidth = paint.measureText(value)

        if (text.isEmpty()) {
            offset = 0f
            stopScroll()
            invalidate()
            return
        }

        val viewWidth = width.toFloat()
        if (viewWidth <= 0f) {
            // 还没测量完，等 onSizeChanged 再处理
            invalidate()
            return
        }

        if (streaming) {
            // 流式：内容变长时保持「右端可见」—— 新字立刻能看到。
            // 内容变短则回到起点（说明是切句，不是追加）。
            if (textWidth >= oldWidth) {
                offset = maxOf(0f, textWidth - viewWidth)
            } else {
                offset = 0f
            }
            invalidate()
            return
        }

        // 定行：文本没超宽就贴左；超宽则从 0 开始滚（marquee 循环）。
        offset = 0f
        invalidate()
        ensureScrolling()
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val w = MeasureSpec.getSize(widthMeasureSpec)
        val fm = paint.fontMetrics
        val h = (fm.descent - fm.ascent).toInt() + paddingTop + paddingBottom
        setMeasuredDimension(w, h.coerceAtLeast(1))
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        if (w != oldw) {
            // 宽度变了（旋转屏幕）：滚动范围要重算
            offset = minOf(offset, maxOf(0f, textWidth - w.toFloat()))
            ensureScrolling()
        }
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        if (text.isEmpty()) return
        val fm = paint.fontMetrics
        val baseline = paddingTop - fm.ascent
        canvas.save()
        canvas.translate(-offset, 0f)
        canvas.drawText(text, paddingLeft.toFloat(), baseline, paint)
        canvas.restore()
    }

    /** 推进一帧的滚动。 */
    private fun advance(dtMs: Long) {
        if (dtMs <= 0L) return
        val viewWidth = width.toFloat()
        if (viewWidth <= 0f || textWidth <= viewWidth) return

        val maxOffset = textWidth - viewWidth
        offset += speedPxPerSec * (dtMs / 1000f)

        if (offset >= maxOffset) {
            // 到底 → 停顿
            if (!pausedAtEnd) {
                pausedAtEnd = true
                pauseUntil = System.currentTimeMillis() + endPauseMs
            }
            if (System.currentTimeMillis() >= pauseUntil) {
                // 停顿结束 → 滚回开头
                pausedAtEnd = false
                offset = 0f
            } else {
                offset = maxOffset
            }
        }
    }

    private var pausedAtEnd = false
    private var pauseUntil = 0L

    /** 需要滚动时才启动帧循环（静止时不耗电）。 */
    private fun ensureScrolling() {
        val viewWidth = width.toFloat()
        val need = viewWidth > 0f && textWidth > viewWidth
        if (need && !running) {
            running = true
            lastFrameNs = 0L
            Choreographer.getInstance().postFrameCallback(frameCallback)
        } else if (!need && running) {
            stopScroll()
        }
    }

    fun stopScroll() {
        running = false
        pausedAtEnd = false
        Choreographer.getInstance().removeFrameCallback(frameCallback)
    }

    override fun onDetachedFromWindow() {
        stopScroll()
        super.onDetachedFromWindow()
    }
}
