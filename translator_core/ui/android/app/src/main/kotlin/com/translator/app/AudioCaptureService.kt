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
        const val EXTRA_SOURCE_INDEX = "source_index"
        const val EXTRA_PROJECTION_RESULT = "projection_result"

        /** 待注入的 MediaProjection（授权 Activity 先于服务就绪时暂存） */
        @Volatile
        var pendingProjection: MediaProjection? = null

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
        var activeMode: String = "idle"   // idle | systemAudio | microphone

        @Volatile
        var degradeReason: String = ""

        fun start(context: Context, useSystemAudio: Boolean, session: Long, sourceIndex: Int) {
            val intent = Intent(context, AudioCaptureService::class.java)
                .putExtra(EXTRA_USE_SYSTEM_AUDIO, useSystemAudio)
                .putExtra(EXTRA_SESSION, session)
                .putExtra(EXTRA_SOURCE_INDEX, sourceIndex)
            context.startForegroundService(intent)
        }

        /**
         * 投屏授权后就起内放服务。
         *
         * projection 由 MediaProjectionManager.getMediaProjection 构造，
         * 与 startForegroundService 的 MEDIA_PROJECTION 类型要求配对：
         * **必须先拿到 projection 再 startForeground**（Android 14 强制），
         * 故 projection 经静态暂存传递（同进程）。
         */
        fun startWithProjection(
            context: Context,
            session: Long,
            sourceIndex: Int,
            resultCode: Int,
            data: Intent,
        ) {
            val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE)
                as MediaProjectionManager
            pendingProjection = mgr.getMediaProjection(resultCode, data)
            val intent = Intent(context, AudioCaptureService::class.java)
                .putExtra(EXTRA_USE_SYSTEM_AUDIO, true)
                .putExtra(EXTRA_SESSION, session)
                .putExtra(EXTRA_SOURCE_INDEX, sourceIndex)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, AudioCaptureService::class.java))
        }
    }

    private var audioRecord: AudioRecord? = null
    /** Dart 会话指针（0 = 无效，绝不启动）。所有权与生命周期归 Dart。 */
    @Volatile private var sessionPtr: Long = 0L
    private var sourceIndex: Int = 0
    private var captureThread: Thread? = null
    @Volatile private var running = false
    private var mediaProjection: MediaProjection? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val useSystemAudio = intent?.getBooleanExtra(EXTRA_USE_SYSTEM_AUDIO, false) ?: false
        sessionPtr = intent?.getLongExtra(EXTRA_SESSION, 0L) ?: 0L
        sourceIndex = intent?.getIntExtra(EXTRA_SOURCE_INDEX, 0) ?: 0

        if (sessionPtr == 0L) {
            Log.e(TAG, "无会话指针（Dart 未 start 或传丢），拒绝启动采集")
            stopSelf()
            return START_NOT_STICKY
        }

        // 接管投屏（先于 startForeground —— Android 14 对
        // FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION 的要求：
        // startForeground 时 projection 必须已就绪，否则抛
        // ForegroundServiceStartNotAllowedException）
        mediaProjection = pendingProjection
        pendingProjection = null

        // 前台服务类型（API 29+）
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val type = if (useSystemAudio && mediaProjection != null) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            } else {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            }
            startForeground(NOTIFICATION_ID, buildNotification(), type)
        } else {
            startForeground(NOTIFICATION_ID, buildNotification())
        }

        startCapture(useSystemAudio && mediaProjection != null)
        return START_STICKY   // 被杀后自动重启（容器场景尤其需要）
    }

    /**
     * 启动采集。优先内放，失败则降级麦克风。
     *
     * 降级是**设计内行为**而非异常：卓易通容器内内放不可用是已知情况。
     */
    private fun startCapture(useSystemAudio: Boolean) {
        if (running) return

        if (useSystemAudio && startSystemAudioCapture()) {
            activeMode = "systemAudio"
            degradeReason = ""
        } else {
            if (useSystemAudio) {
                degradeReason = "内放采集不可用，已降级为麦克风"
                Log.w(TAG, "内放采集失败：$degradeReason")
            }
            if (!startMicCapture()) {
                Log.e(TAG, "麦克风采集也无法启动")
                activeMode = "idle"
                stopSelf()
                return
            }
            activeMode = "microphone"
        }

        // 把授权结果回灌 Rust 会话：capability 随真实状态更新
        // （麦克风恒可用；内放仅在 projection 成功时可用）
        NativeBridge.sessionSetCaptureGranted(
            sessionPtr,
            activeMode == "systemAudio" || activeMode == "microphone",
        )

        running = true
        startReadThread()
    }

    /**
     * 系统内放捕获。
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

            val minBuf = AudioRecord.getMinBufferSize(
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_FLOAT
            )
            if (minBuf <= 0) return false

            val recorder = AudioRecord.Builder()
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
                        .setSampleRate(SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_IN_MONO)
                        .build()
                )
                .setBufferSizeInBytes(minBuf * 4)
                .setAudioPlaybackCaptureConfig(config)
                .build()

            if (recorder.state != AudioRecord.STATE_INITIALIZED) {
                recorder.release()
                return false
            }
            recorder.startRecording()
            if (recorder.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                recorder.release()
                return false
            }
            audioRecord = recorder
            true
        } catch (e: SecurityException) {
            // 无 MANAGE_AUDIO_CAPTURE 权限（普通应用默认无）
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
     * 麦克风采集（降级路径）。
     *
     * 用 VOICE_COMMUNICATION 而非 MIC —— 前者启用 AEC + 降噪，
     * 在外放场景下能减少自己声音的回声泄漏。
     */
    private fun startMicCapture(): Boolean {
        return try {
            val minBuf = AudioRecord.getMinBufferSize(
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_FLOAT
            )
            if (minBuf <= 0) return false

            val recorder = AudioRecord.Builder()
                .setAudioSource(MediaRecorder.AudioSource.VOICE_COMMUNICATION)
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
                        .setSampleRate(SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_IN_MONO)
                        .build()
                )
                .setBufferSizeInBytes(minBuf * 4)
                .build()

            if (recorder.state != AudioRecord.STATE_INITIALIZED) {
                recorder.release()
                return false
            }
            recorder.startRecording()
            if (recorder.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                recorder.release()
                return false
            }
            audioRecord = recorder
            true
        } catch (e: SecurityException) {
            Log.e(TAG, "无 RECORD_AUDIO 权限", e)
            false
        }
    }

    /**
     * 读取线程。
     *
     * 采样数 = 16000 × 0.020 = 320，与 Rust 侧 CHUNK_SAMPLES 一致。
     * 块长必须三端统一（DESIGN §7.2），否则破坏黄金测试前提。
     */
    private fun startReadThread() {
        captureThread = thread(name = "audio-capture", priority = Thread.MAX_PRIORITY) {
            val samplesPerChunk = SAMPLE_RATE * CHUNK_MS / 1000
            val buffer = FloatArray(samplesPerChunk)
            val recorder = audioRecord ?: return@thread

            Log.i(TAG, "采集线程启动，模式=$activeMode，块长=${samplesPerChunk}样本")

            while (running) {
                var filled = 0
                while (filled < samplesPerChunk && running) {
                    val n = recorder.read(
                        buffer,
                        filled,
                        samplesPerChunk - filled,
                        AudioRecord.READ_BLOCKING
                    )
                    when {
                        n > 0 -> filled += n
                        n == 0 -> Thread.sleep(5)
                        // ERROR_INVALID_OPERATION / ERROR_BAD_VALUE
                        else -> {
                            Log.e(TAG, "AudioRecord.read 返回错误：$n")
                            running = false
                            break
                        }
                    }
                }
                if (filled == samplesPerChunk) {
                    // 按「会话指针 + 声源索引」推入 Rust 队列。
                    // ⚠️ 非阻塞：Rust 队列满时只丢音频，绝不阻塞音频线程
                    //（见 DESIGN §6.2「捕获线程绝不能被下游阻塞」）。
                    val rc = NativeBridge.sessionPushChunk(sessionPtr, sourceIndex, buffer)
                    if (rc != 0) {
                        Log.w(TAG, "推入 Rust 失败：$rc")
                    }
                }
            }
            Log.i(TAG, "采集线程退出")
        }
    }

    /**
     * 停止采集并**等待**读线程退出。
     *
     * Dart 侧 stop 编排依赖本方法的同步性：回包之后才会 session_free，
     * 线程若还在跑就会对已释放会话推送 → UAF。join 超时是兜底
     * （读线程阻塞在 recorder.read 时最多等 1s，随后 release 打断）。
     */
    fun stopAndAwait() {
        running = false
        val t = captureThread
        captureThread = null
        audioRecord?.let {
            try {
                if (it.recordingState == AudioRecord.RECORDSTATE_RECORDING) it.stop()
            } catch (_: Exception) {}
            it.release()
        }
        audioRecord = null
        t?.join(1000)
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
            "systemAudio" -> "正在转写系统音频"
            "microphone" -> "正在转写麦克风（降级模式）"
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
