package com.translator.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.Log
import kotlin.concurrent.thread

/**
 * 音频采集前台服务。
 *
 * ## 两条采集路径
 *
 * | 路径 | 实现 | 说明 |
 * |---|---|---|
 * | 系统内放 | `AudioRecord` + `AudioPlaybackCaptureConfiguration` | 需 MediaProjection 授权；DRM 内容必然静音 |
 * | 麦克风 | `AudioRecord` + `MediaRecorder.AudioSource.VOICE_COMMUNICATION` | 降级路径，带 AEC + 降噪 |
 *
 * ## 采集句柄（2026-10-26 接线改造）
 *
 * 句柄**不再由本服务创建**：Rust 会话（Dart 拥有）在建会话时创建采集器，
 * Kotlin 只是它的外部生产者。start 参数里传入 `session`（会话指针，
 * Dart 侧已 start 完成）与 `sourceIndex`（多声源时下标）。
 * 音频经 [NativeBridge.sessionPushChunk] 按索引即时查表推入 ——
 * 会话重建采集器后索引依旧有效，不存在悬垂句柄。
 *
 * ## 为什么必须前台服务、且必须同进程
 *
 * Android 8+ 禁止后台启动；14+ 要求 `foregroundServiceType` 声明。
 * ⚠️ 本服务与 Dart/会话**必须同进程**（Manifest 已去掉 :capture）：
 * 会话指针是主进程地址空间的值，跨进程解引用必崩。卓易通休眠保活
 * 是远期需求，届时改共享内存 relay，勿恢复进程隔离。
 *
 * ## ⚠️ Android 14 的 MediaProjection 时序（2026-10-27 闪退教训）
 *
 * 官方硬规定：**先启动 `mediaProjection` 类型的前台服务，
 * 再调 `getMediaProjection()`**。顺序反了就在 getMediaProjection
 * 当场抛 SecurityException（实测崩溃栈：
 * "Media projections require a foreground service of type
 * ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION"）。
 * 因此授权结果（resultCode + data Intent）经 Intent extras 传给本服务，
 * projection 在 startForeground **成功之后**由服务自己创建。
 * 旧实现把 getMediaProjection 放在 Activity 的 onActivityResult 里
 * （经静态暂存 pendingProjection 交接）——已废弃，勿恢复。
 *
 * ## 卓易通容器注意事项
 *
 * 容器内内放路径**大概率不可用**（见 docs/DESIGN.md §2.5）：
 * MediaProjection 需宿主代理 / audio namespace 隔离 / Manifest 权限被审计。
 * 内放失败必须**优雅降级**到麦克风并回报 degradeReason（禁止静默降级）。
 */
class AudioCaptureService : Service() {

    companion object {
        private const val TAG = "TranslatorCapture"
        private const val CHANNEL_ID = "translator_capture"
        private const val NOTIFICATION_ID = 1001

        /** 与 Rust 侧 AppConfig 对齐 */
        const val SAMPLE_RATE = 16000
        const val CHANNEL_COUNT = 1
        const val CHUNK_MS = 20

        const val EXTRA_USE_SYSTEM_AUDIO = "use_system_audio"
        const val EXTRA_SESSION = "session"
        // ── 双路并行采集（2026-10-28 用户实测「麦克风收音不到」后改造）──
        // 旧设计：会话建两路采集器，但服务只起一路 AudioRecord ——
        // 默认走内放时麦克风那路**根本没采**，用户对着手机说话无声。
        // 新设计：sysIndex / micIndex 各自独立（-1 = 该路不起），
        // 两路各自 AudioRecord + 读线程 + 推流下标，可同时运行。
        // Android 14 允许 MediaProjection 内放与麦克风并行。
        const val EXTRA_SYS_INDEX = "sys_index"
        const val EXTRA_MIC_INDEX = "mic_index"
        const val EXTRA_PROJECTION_RESULT_CODE = "projection_result_code"
        const val EXTRA_PROJECTION_DATA = "projection_data"

        /**
         * 2026-10-27：`pendingProjection` 静态暂存**已删除**。
         * Android 14 禁止在「非 mediaProjection 前台服务」里调
         * getMediaProjection()（当场抛 SecurityException）——
         * projection 必须由本服务在 startForeground 成功后自行创建，
         * 见 [startWithProjectionResult]。
         */
        @Volatile
        var instance: AudioCaptureService? = null

        /**
         * 是否运行在卓易通容器内。
         *
         * 容器内限制定制（DESIGN §2.5）：权限需逐项手动授予、
         * 内放采集大概率不可用、需前台服务保活。
         * 探测：容器里没有标准 Google 服务框架，Build 指纹异常。
         */
        @Volatile
        var isZhuoYiTong: Boolean = false

        /** 采集状态回调，供 Flutter 侧查询 */
        @Volatile
        var activeMode: String = "idle"   // idle | systemAudio | microphone | dual

        @Volatile
        var degradeReason: String = ""

        /**
         * 声源类型编号（与 Dart 侧 / Rust FFI `translator_session_set_source_enabled`
         * 的 `kind` 参数**必须一致**）。
         */
        const val KIND_SYSTEM_AUDIO = 0
        const val KIND_MICROPHONE = 1

        /**
         * 启动采集服务（双路参数版）。
         *
         * @param sysIndex 内放路的 Rust 采集器下标（-1 = 不起内放）
         * @param micIndex 麦克风路的 Rust 采集器下标（-1 = 不起麦克风）
         */
        fun start(
            context: Context,
            useSystemAudio: Boolean,
            session: Long,
            sysIndex: Int,
            micIndex: Int,
        ) {
            val intent = Intent(context, AudioCaptureService::class.java)
                .putExtra(EXTRA_USE_SYSTEM_AUDIO, useSystemAudio)
                .putExtra(EXTRA_SESSION, session)
                .putExtra(EXTRA_SYS_INDEX, sysIndex)
                .putExtra(EXTRA_MIC_INDEX, micIndex)
            context.startForegroundService(intent)
        }

        /**
         * 投屏授权后就起采集服务（Android 14 正确时序）。
         *
         * ⚠️ 只传 resultCode + data，**不传 MediaProjection 对象**：
         * Android 14 规定 getMediaProjection() 必须在
         * mediaProjection 类型前台服务已 startForeground 之后调用，
         * 在 Activity 里先建再塞给服务 = 当场 SecurityException 闪退
         * （2026-10-27 实测崩溃栈）。projection 的创建点见 onStartCommand。
         */
        fun startWithProjectionResult(
            context: Context,
            session: Long,
            sysIndex: Int,
            micIndex: Int,
            resultCode: Int,
            data: Intent,
        ) {
            val intent = Intent(context, AudioCaptureService::class.java)
                .putExtra(EXTRA_USE_SYSTEM_AUDIO, true)
                .putExtra(EXTRA_SESSION, session)
                .putExtra(EXTRA_SYS_INDEX, sysIndex)
                .putExtra(EXTRA_MIC_INDEX, micIndex)
                .putExtra(EXTRA_PROJECTION_RESULT_CODE, resultCode)
                .putExtra(EXTRA_PROJECTION_DATA, data)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, AudioCaptureService::class.java))
        }
    }

    // ── 双路并行采集（2026-10-28 改造）──────────────────
    //
    // 每路 = 一个 AudioRecord + 一个读线程 + 一个 Rust 推流下标。
    // 内放路要 MediaProjection；麦克风路只要 RECORD_AUDIO。
    // 两路互不拖累：内放失败不影响麦克风继续（反之亦然）。
    private var sysRecorder: AudioRecord? = null
    private var micRecorder: AudioRecord? = null
    private var sysThread: Thread? = null
    private var micThread: Thread? = null
    @Volatile private var sysRunning = false
    @Volatile private var micRunning = false
    private var sysIndex = -1
    private var micIndex = -1
    /** Dart 会话指针（0 = 无效，绝不启动）。所有权与生命周期归 Dart。 */
    @Volatile private var sessionPtr: Long = 0L
    private var mediaProjection: MediaProjection? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val useSystemAudio = intent?.getBooleanExtra(EXTRA_USE_SYSTEM_AUDIO, false) ?: false
        sessionPtr = intent?.getLongExtra(EXTRA_SESSION, 0L) ?: 0L
        sysIndex = intent?.getIntExtra(EXTRA_SYS_INDEX, -1) ?: -1
        micIndex = intent?.getIntExtra(EXTRA_MIC_INDEX, -1) ?: -1

        if (sessionPtr == 0L) {
            Log.e(TAG, "无会话指针（Dart 未 start 或传丢），拒绝启动采集")
            stopSelf()
            return START_NOT_STICKY
        }
        if (sysIndex < 0 && micIndex < 0) {
            Log.e(TAG, "两路下标都无效，无处推流")
            stopSelf()
            return START_NOT_STICKY
        }

        // Android 14+ 时序（2026-10-27 闪退教训，勿改回旧顺序）：
        //   ① 先 startForeground(MEDIA_PROJECTION) —— 服务就位
        //   ② 再 getMediaProjection(resultCode, data) —— 此刻才合法
        val projCode = intent?.getIntExtra(EXTRA_PROJECTION_RESULT_CODE, 0) ?: 0
        @Suppress("DEPRECATION")
        val projData: Intent? =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU)
                intent?.getParcelableExtra(EXTRA_PROJECTION_DATA, Intent::class.java)
            else intent?.getParcelableExtra(EXTRA_PROJECTION_DATA)

        // 前台服务类型：两路都要 = 复合类型（Android 10+ 支持多类型）
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val wantSys = useSystemAudio && projData != null && sysIndex >= 0
            val type = if (wantSys && micIndex >= 0) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION or
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            } else if (wantSys) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            } else {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            }
            startForeground(NOTIFICATION_ID, buildNotification(), type)
        } else {
            startForeground(NOTIFICATION_ID, buildNotification())
        }

        // startForeground 成功之后才允许创建 projection
        var projectionOk = false
        if (useSystemAudio && projData != null) {
            try {
                val mgr = getSystemService(Context.MEDIA_PROJECTION_SERVICE)
                    as MediaProjectionManager
                mediaProjection = mgr.getMediaProjection(projCode, projData)
                mediaProjection?.registerCallback(object : MediaProjection.Callback() {
                    override fun onStop() {
                        Log.w(TAG, "MediaProjection 被系统回收，内放路下线")
                        AudioCaptureService.degradeReason = "投屏授权已被系统收回，内放停止"
                        // 只停内放路，麦克风继续（双路独立）
                        stopSystemAudio()
                    }
                }, null)
                projectionOk = true
            } catch (e: Exception) {
                Log.e(TAG, "getMediaProjection 失败：${e.message}", e)
                mediaProjection = null
            }
        }

        startCapture(useSystemAudio && projectionOk)
        return START_NOT_STICKY
    }

    /**
     * 启动两路采集（各自独立成败）。
     *
     * @param wantSystemAudio 用户要内放且 projection 就绪
     * 内放失败**不再降级掉麦克风** —— 双路时代两路本来就并行，
     * 内放挂了麦克风那路照常，degradeReason 如实记录即可。
     */
    private fun startCapture(wantSystemAudio: Boolean) {
        var sysUp = false
        if (wantSystemAudio && sysIndex >= 0) {
            sysUp = startSystemAudioCapture()
            if (!sysUp) {
                degradeReason = "内放采集不可用（会议声音采不到），麦克风正常"
                Log.w(TAG, "内放采集失败：$degradeReason")
            }
        }
        var micUp = false
        if (micIndex >= 0) {
            micUp = startMicCapture()
            if (!micUp) {
                Log.e(TAG, "麦克风采集启动失败")
                if (micIndex >= 0 && degradeReason.isEmpty()) {
                    degradeReason = "麦克风采集启动失败"
                }
            }
        }

        if (!sysUp && !micUp) {
            Log.e(TAG, "两路都起不来，服务退出")
            activeMode = "idle"
            stopSelf()
            return
        }

        activeMode = when {
            sysUp && micUp -> "dual"
            sysUp -> "systemAudio"
            else -> "microphone"
        }
        degradeReason = if (activeMode == "dual") "" else degradeReason

        // 把授权结果回灌 Rust 会话：capability 随真实状态更新。
        // ⚠️ 包 try/catch：JNI 桥接层异常（如库未就绪）不该杀死服务主线程
        try {
            NativeBridge.sessionSetCaptureGranted(sessionPtr, true)
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "sessionSetCaptureGranted 不可用（原生库未加载）", e)
        } catch (e: Exception) {
            Log.e(TAG, "sessionSetCaptureGranted 异常", e)
        }

        if (sysUp) startReadThread(sys = true)
        if (micUp) startReadThread(sys = false)
        updateNotification()
    }

    /**
     * 系统内放捕获（AudioPlaybackCapture）。
     *
     * 三个必要条件：
     * 1. MediaProjection 已授权（需 Activity 发起）
     * 2. API 29+（AudioPlaybackCapture 引入版本）
     * 3. 目标 App 未 opt-out、未 DRM 保护
     */
    private fun startSystemAudioCapture(): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return false
        val proj = mediaProjection ?: return false

        return try {
            val config = AudioPlaybackCaptureConfiguration.Builder(proj)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                .build()

            val recorder = buildRecorder(config) ?: return false
            recorder.startRecording()
            if (recorder.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                recorder.release()
                return false
            }
            sysRecorder = recorder
            true
        } catch (e: SecurityException) {
            Log.w(TAG, "缺少音频捕获权限：${e.message}")
            false
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "AudioPlaybackCapture 不被支持：${e.message}")
            false
        } catch (e: Exception) {
            Log.e(TAG, "内放采集异常", e)
            false
        }
    }

    /**
     * 麦克风采集。
     *
     * 用 VOICE_COMMUNICATION 而非 MIC —— 前者启用 AEC + 降噪，
     * 在外放场景下能减少自己声音的回声泄漏。
     */
    private fun startMicCapture(): Boolean {
        return try {
            val recorder = buildRecorder(null) ?: return false
            recorder.startRecording()
            if (recorder.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                recorder.release()
                return false
            }
            micRecorder = recorder
            true
        } catch (e: SecurityException) {
            Log.e(TAG, "无 RECORD_AUDIO 权限", e)
            false
        }
    }

    /**
     * 构建 AudioRecord（两路共用的格式约定）。
     *
     * @param captureConfig null = 麦克风（VOICE_COMMUNICATION）；
     *                      非 null = 内放（AudioPlaybackCapture）
     */
    private fun buildRecorder(
        captureConfig: AudioPlaybackCaptureConfiguration?,
    ): AudioRecord? {
        val minBuf = AudioRecord.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_FLOAT,
        )
        if (minBuf <= 0) return null
        val fmt = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_MONO)
            .build()
        val builder = if (captureConfig != null) {
            AudioRecord.Builder()
                .setAudioPlaybackCaptureConfig(captureConfig)
                .setAudioFormat(fmt)
        } else {
            AudioRecord.Builder()
                .setAudioSource(MediaRecorder.AudioSource.VOICE_COMMUNICATION)
                .setAudioFormat(fmt)
        }
        val recorder = builder.setBufferSizeInBytes(minBuf * 4).build()
        if (recorder.state != AudioRecord.STATE_INITIALIZED) {
            recorder.release()
            return null
        }
        return recorder
    }

    /**
     * 读取线程（每路一条）。
     *
     * 采样数 = 16000 × 0.020 = 320，与 Rust 侧 CHUNK_SAMPLES 一致。
     * 块长必须三端统一（DESIGN §7.2），否则破坏黄金测试前提。
     */
    private fun startReadThread(sys: Boolean) {
        val name = if (sys) "audio-capture-sys" else "audio-capture-mic"
        // ⚠️ running 标志必须在**线程体外**置位（2026-10-28 修复）。
        //
        // 原来它在**线程体内第一行**设置，而线程是异步启动的 →
        // 调用方（startCapture / setSourceEnabled）返回时标志可能还是
        // false → 随后的状态判定会误判成「这一路没起来」。
        // 线程体自己也会置位（幂等），但线程外的这次才是**同步可见**的。
        if (sys) sysRunning = true else micRunning = true
        val thread = thread(name = name, priority = Thread.MAX_PRIORITY) {
            val samplesPerChunk = SAMPLE_RATE * CHUNK_MS / 1000
            val buffer = FloatArray(samplesPerChunk)
            val recorder = if (sys) sysRecorder else micRecorder
            val index = if (sys) sysIndex else micIndex

            Log.i(TAG, "采集线程启动（${if (sys) "内放" else "麦克风"}），下标=$index，块长=${samplesPerChunk}样本")

            while (if (sys) sysRunning else micRunning) {
                var filled = 0
                while (filled < samplesPerChunk && (if (sys) sysRunning else micRunning)) {
                    val n = recorder?.read(
                        buffer,
                        filled,
                        samplesPerChunk - filled,
                        AudioRecord.READ_BLOCKING,
                    ) ?: -1
                    when {
                        n > 0 -> filled += n
                        n == 0 -> Thread.sleep(5)
                        else -> {
                            Log.e(TAG, "AudioRecord.read 返回错误：$n（${if (sys) "内放" else "麦克风"}）")
                            if (sys) sysRunning = false else micRunning = false
                            break
                        }
                    }
                }
                if (filled == samplesPerChunk && recorder != null) {
                    // 按「会话指针 + 声源索引」推入 Rust 队列。
                    // ⚠️ 非阻塞：Rust 队列满时只丢音频，绝不阻塞音频线程
                    //（见 DESIGN §6.2「捕获线程绝不能被下游阻塞」）。
                    try {
                        val rc = NativeBridge.sessionPushChunk(sessionPtr, index, buffer)
                        if (rc != 0) {
                            Log.w(TAG, "推入 Rust 失败：$rc（${if (sys) "内放" else "麦克风"}）")
                        }
                    } catch (e: UnsatisfiedLinkError) {
                        Log.e(TAG, "原生库未加载，本路退出", e)
                        if (sys) sysRunning = false else micRunning = false
                    }
                }
            }
            Log.i(TAG, "采集线程退出（${if (sys) "内放" else "麦克风"}）")
        }
        if (sys) sysThread = thread else micThread = thread
    }

    /** 只停内放路（projection 被系统回收时用），麦克风继续。 */
    private fun stopSystemAudio() {
        stopSource(KIND_SYSTEM_AUDIO, keepServiceAlive = false)
    }

    /**
     * 停掉一路采集（2026-10-28 新增：支撑运行时实时切换采集源）。
     *
     * @param kind 0 = 系统内放，1 = 麦克风
     * @param keepServiceAlive true = 即使两路都停了也**不**退出服务。
     *
     *   运行时切换用 true：用户可能「全关再全开」，服务先留着，
     *   否则 `stopSelf()` 之后服务已销毁，再打开某路就无处可去。
     *   内部降级（如 projection 被回收）用 false —— 那时是真的没救了。
     */
    private fun stopSource(kind: Int, keepServiceAlive: Boolean) {
        val label = if (kind == KIND_SYSTEM_AUDIO) "内放" else "麦克风"
        if (kind == KIND_SYSTEM_AUDIO) {
            sysRunning = false
            sysRecorder?.let {
                try { if (it.recordingState == AudioRecord.RECORDSTATE_RECORDING) it.stop() } catch (_: Exception) {}
                it.release()
            }
            sysRecorder = null
            sysThread?.join(1000)
            sysThread = null
        } else {
            micRunning = false
            micRecorder?.let {
                try { if (it.recordingState == AudioRecord.RECORDSTATE_RECORDING) it.stop() } catch (_: Exception) {}
                it.release()
            }
            micRecorder = null
            micThread?.join(1000)
            micThread = null
        }
        Log.i(TAG, "已停止$label 路采集")
        refreshActiveMode(keepServiceAlive)
    }

    /**
     * 按当前两路状态刷新采集模式（activeMode）+ 通知。
     *
     * 单独抽出来是因为「运行时切换」与「内部降级」都要用到，
     * 且判定口径必须一致 —— 否则会出现「明明麦克风在跑，
     * 通知里却显示 idle」这种自相矛盾的状态。
     */
    private fun refreshActiveMode(keepServiceAlive: Boolean) {
        val sysUp = sysRunning && sysRecorder != null
        val micUp = micRunning && micRecorder != null
        activeMode = when {
            sysUp && micUp -> "dual"
            sysUp -> "systemAudio"
            micUp -> "microphone"
            else -> "idle"
        }
        if (activeMode == "idle" && !keepServiceAlive) {
            Log.i(TAG, "两路采集都已停止，服务退出")
            stopSelf()
        }
        updateNotification()
    }

    /**
     * 运行时启停某一路采集（2026-10-28 用户需求「转写过程中实时切换采集源」）。
     *
     * @param kind 0 = 系统内放，1 = 麦克风
     * @param enabled true = 启用该路，false = 停用该路
     * @return true = 操作成功（该路确实处于期望状态）
     *
     * ## 为什么可以在运行中切换
     *
     * 两路采集本来就是**各自独立**的 AudioRecord + 读线程
     * （见本文件「双路并行采集」段），互不拖累。
     * 停一路 = 停它的 recorder + join 它的读线程；
     * 起一路 = 新建 recorder + 新建读线程。
     *
     * ⚠️ 读线程里 `recorder` 是**闭包捕获**的局部变量，
     *    所以重启某路**必须新建读线程**（旧线程捕获的是已 release
     *    的 recorder），不能复用。
     *
     * ## 与 Rust 侧的关系
     *
     * Rust 侧的采集器（`session.capture`）**始终存在**，
     * 音频经 `sessionPushChunk` 按下标推入对应通道。
     * 某路不推 → 该路接收端一直等不到数据 → 该路不产生字幕。
     * 所以这里只需管住 Kotlin 的生产者，**Rust 侧无需改动**
     * （Android 的 Rust 采集器是 `ExternalPushCapture`，start/stop 皆空实现）。
     */
    fun setSourceEnabled(kind: Int, enabled: Boolean): Boolean {
        val isSys = kind == KIND_SYSTEM_AUDIO
        val label = if (isSys) "内放" else "麦克风"

        // 该路在本会话里根本没有采集器（下标无效）→ 无从启停
        val index = if (isSys) sysIndex else micIndex
        if (index < 0) {
            Log.w(TAG, "切换$label 路失败：本会话没有该路的采集器")
            return false
        }

        if (!enabled) {
            stopSource(kind, keepServiceAlive = true)
            return true
        }

        // 已经是开着的 → 幂等返回成功
        val alreadyUp = if (isSys) sysRunning && sysRecorder != null
        else micRunning && micRecorder != null
        if (alreadyUp) return true

        val ok = if (isSys) {
            if (mediaProjection == null) {
                // 用户从未授权投屏（或已被回收）→ 无法开内放。
                // 如实回报，不静默失败（DESIGN §2.5.2）。
                AudioCaptureService.degradeReason = "内放需要投屏授权，请重新授权后再开启"
                Log.w(TAG, "切换内放路失败：缺少 MediaProjection 授权")
                false
            } else {
                startSystemAudioCapture()
            }
        } else {
            startMicCapture()
        }

        if (!ok) {
            if (isSys) {
                AudioCaptureService.degradeReason = "内放采集不可用（会议声音采不到）"
            } else {
                Log.e(TAG, "切换麦克风路失败：AudioRecord 起不来")
                if (AudioCaptureService.degradeReason.isEmpty()) {
                    AudioCaptureService.degradeReason = "麦克风采集启动失败"
                }
            }
            refreshActiveMode(keepServiceAlive = true)
            return false
        }

        // 起读线程（必须新建：旧线程捕获的是已 release 的 recorder）
        startReadThread(sys = isSys)
        Log.i(TAG, "已启用$label 路采集")
        refreshActiveMode(keepServiceAlive = true)
        return true
    }

    /**
     * 停止采集并**等待**两路读线程退出。
     *
     * Dart 侧 stop 编排依赖本方法的同步性：回包之后才会 session_free，
     * 线程若还在跑就会对已释放会话推送 → UAF。join 超时是兜底
     * （读线程阻塞在 recorder.read 时最多等 1s，随后 release 打断）。
     */
    fun stopAndAwait() {
        sysRunning = false
        micRunning = false
        for (r in listOf(sysRecorder, micRecorder)) {
            r?.let {
                try {
                    if (it.recordingState == AudioRecord.RECORDSTATE_RECORDING) it.stop()
                } catch (_: Exception) {}
                it.release()
            }
        }
        sysRecorder = null
        micRecorder = null
        sysThread?.join(1000); sysThread = null
        micThread?.join(1000); micThread = null
    }

    override fun onDestroy() {
        stopAndAwait()
        mediaProjection?.stop()
        mediaProjection = null

        activeMode = "idle"
        instance = null
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    // ── 通知 ────────────────────────────────────────

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "音频采集",
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = "转写期间保持前台运行"
                setShowBadge(false)
            }
            val nm = getSystemService(NotificationManager::class.java)
            nm.createNotificationChannel(channel)
        }
    }

    private fun buildNotification(): Notification {
        val modeText = when (activeMode) {
            "dual" -> "正在转写：会议声音 + 麦克风"
            "systemAudio" -> "正在转写系统音频"
            "microphone" -> "正在转写麦克风"
            else -> "正在准备"
        }
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Translator")
            .setContentText(modeText)
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setOngoing(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .build()
    }

    /**
     * 更新通知（切换模式后调用）。
     */
    fun updateNotification() {
        val nm = getSystemService(NotificationManager::class.java)
        nm.notify(NOTIFICATION_ID, buildNotification())
    }
}
