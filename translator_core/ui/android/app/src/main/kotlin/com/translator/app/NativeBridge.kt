package com.translator.app

import android.util.Log

/**
 * Rust 侧桥接层（JNI）。
 *
 * ## 库名约定（AGENTS.md）
 * crate `translator-core`，产物 `libtranslator_core.so`，
 * 导出符号前缀 `translator_`。
 *
 * ## 为什么手写 JNI 而不用 flutter_rust_bridge
 * 当前阶段先打通「音频采集」这一条链路（最小可验证路径）。
 * FRB 生成的绑定留给后续整体接入 —— 手写只需十几个函数，可控性更高，
 * 且不依赖代码生成工具链。
 *
 * ## 内存约定
 * 每块 320 个 f32 = 1280 字节，约 50 次/秒 = 64 KB/秒。
 * 用 `FloatArray` 直传，不逐样本装箱（避免 GC 压力）。
 *
 * ## ⚠️ 字符串内存（未解决的坑）
 * Rust 的 `translator_capability_desc` / `translator_version` 返回
 * `*mut c_char`，需用 `translator_free_string` 释放。
 * 但 JNI 的 `external fun ... : String` 会在返回时把 C 字符串转成
 * Java String，**原指针丢失** → 无法释放 → 泄漏。
 *
 * 当前用 `nativeFreeString(handle)` 占位（语义不正确）。
 * 正式方案：改为返回 `ByteArray` + 单独的 free 接口，或让 Rust 侧
 * 用线程局部缓存指针。已在 TODO 标注，待接入 FRB 时统一处理。
 */
object NativeBridge {

    private const val TAG = "TranslatorBridge"
    private var loaded = false

    /** 单块样本数（16 kHz × 20 ms = 320，三端一致，DESIGN §7.2） */
    val chunkSamples: Int
        get() {
            ensureLoaded()
            return nativeChunkSamples()
        }

    /** 采样率（16000） */
    val sampleRate: Int
        get() {
            ensureLoaded()
            return nativeSampleRate()
        }

    private fun ensureLoaded() {
        if (loaded) return
        try {
            System.loadLibrary("translator_core")
            loaded = true
            Log.i(TAG, "libtranslator_core.so 加载成功")
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "加载 libtranslator_core.so 失败", e)
            throw e
        }
    }

    // ── 句柄管理 ──────────────────────────────────────

    /**
     * 创建采集句柄。
     *
     * @param source 0=系统音频 1=麦克风
     * @param runtime 0=原生 1=卓易通容器
     * @return 句柄地址，0 表示失败
     */
    fun createCapture(source: Int, runtime: Int): Long {
        ensureLoaded()
        val h = nativeCreateCapture(source, runtime)
        if (h == 0L) {
            Log.e(TAG, "创建采集句柄失败 (source=$source, runtime=$runtime)")
        }
        return h
    }

    fun freeCapture(handle: Long) {
        if (handle == 0L) return
        ensureLoaded()
        nativeFreeCapture(handle)
    }

    // ── 采集控制 ──────────────────────────────────────

    /**
     * 探测能力。返回 0=系统音频 1=麦克风 2=不可用。
     *
     * **不可用时 UI 必须显式提示** —— 禁止静默降级（DESIGN §2.5.2）。
     */
    fun probeCapture(handle: Long): Int {
        if (handle == 0L) return 2
        ensureLoaded()
        return nativeProbeCapture(handle)
    }

    /**
     * 能力描述（含降级原因与替代方案，直接展示给用户）。
     *
     * TODO: 修复字符串内存泄漏（见类注释）
     */
    fun capabilityDesc(handle: Long): String {
        if (handle == 0L) return "采集句柄未创建"
        ensureLoaded()
        return nativeCapabilityDesc(handle) ?: ""
    }

    /**
     * 开始采集。返回 0 表示成功，其它为 FfiError 错误码。
     */
    fun startCapture(handle: Long): Int {
        if (handle == 0L) return 6
        ensureLoaded()
        return nativeStartCapture(handle)
    }

    /** 停止采集 */
    fun stopCapture(handle: Long): Int {
        if (handle == 0L) return 0
        ensureLoaded()
        return nativeStopCapture(handle)
    }

    /**
     * 取一块音频（16 kHz mono f32）。空数组表示当前无数据。
     *
     * 非阻塞 —— Kotlin 侧主动轮询。
     */
    fun pollChunk(handle: Long): FloatArray {
        if (handle == 0L) return FloatArray(0)
        ensureLoaded()
        val n = chunkSamples
        val buf = FloatArray(n)
        val got = nativePollChunk(handle, buf, n)
        return if (got <= 0) FloatArray(0) else buf.copyOf(got)
    }

    /**
     * 推入一段音频到 Rust 采集缓冲（生产者语义）。
     *
     * Android 的 AudioRecord 在 Kotlin 侧，故音频流向是
     * AudioRecord → 本函数 → Rust 缓冲 → pollChunk → Flutter。
     *
     * @return 0 成功，其它为错误码
     */
    fun pushChunk(handle: Long, samples: FloatArray): Int {
        if (handle == 0L || samples.isEmpty()) return 7  // InvalidArg
        ensureLoaded()
        return nativePushChunk(handle, samples, samples.size)
    }

    /** 丢帧数（健康度） */
    fun droppedFrames(handle: Long): Long {
        if (handle == 0L) return 0
        ensureLoaded()
        return nativeDroppedFrames(handle)
    }

    // ── 会话级（Android 注入链路推荐路径）──────────────
    //
    // 会话指针由 Dart 侧创建并持有（Dart FFI 与本 JNI 加载的是**同一个**
    // libtranslator_core.so —— 同进程 dlopen 返回同一句柄）。
    // Kotlin 只借用，绝不释放。

    /**
     * 按声源索引推入音频到 Dart 会话（320 样本/块，16 kHz mono）。
     *
     * 与 `pushChunk(handle,...)` 的区别：本函数即时在会话表按索引查表，
     * 会话 start 重建采集器后依旧有效 —— 规避句柄代际悬垂。
     *
     * @return 0 成功，其它为错误码（队列满=丢弃本批，绝不重试阻塞）
     */
    fun sessionPushChunk(session: Long, sourceIndex: Int, samples: FloatArray): Int {
        if (session == 0L || samples.isEmpty()) return 7  // InvalidArg
        ensureLoaded()
        return nativeSessionPushChunk(session, sourceIndex.toLong(), samples, samples.size)
    }

    /** 回传采集授权状态（MediaProjection 授予/撤销）。 */
    fun sessionSetCaptureGranted(session: Long, granted: Boolean): Int {
        if (session == 0L) return 7
        ensureLoaded()
        return nativeSessionSetCaptureGranted(session, if (granted) 1 else 0)
    }

    /** 库版本串 */
    fun version(): String {
        ensureLoaded()
        return nativeVersion() ?: "unknown"
    }

    /** 是否完全离线（UI 展示"离线运行"标识） */
    fun isOffline(): Boolean {
        ensureLoaded()
        return nativeIsOffline()
    }

    // ── 原生声明 ──────────────────────────────────────
    // 签名必须与 core/src/ffi.rs 的 #[no_mangle] 导出严格一致。
    //
    // 注意：Rust 侧 `translator_poll_chunk` 的 out 参数是
    // `*mut f32`，JNI 用 FloatArray + GetFloatArrayElements 桥接。
    // Rust 侧 `translator_free_string` 收 *mut c_char，
    // 但 JNI 拿不到原指针，故暂不导出（见类注释 TODO）。

    private external fun nativeCreateCapture(source: Int, runtime: Int): Long
    private external fun nativeFreeCapture(handle: Long)
    private external fun nativeProbeCapture(handle: Long): Int
    private external fun nativeCapabilityDesc(handle: Long): String?
    private external fun nativeStartCapture(handle: Long): Int
    private external fun nativeStopCapture(handle: Long): Int
    private external fun nativePollChunk(handle: Long, out: FloatArray, cap: Int): Int
    private external fun nativePushChunk(handle: Long, samples: FloatArray, count: Int): Int
    private external fun nativeChunkSamples(): Int
    private external fun nativeSampleRate(): Int
    private external fun nativeDroppedFrames(handle: Long): Long
    private external fun nativeVersion(): String?
    private external fun nativeIsOffline(): Boolean
    private external fun nativeSessionPushChunk(
        session: Long, sourceIndex: Long, samples: FloatArray, count: Int): Int
    private external fun nativeSessionSetCaptureGranted(session: Long, granted: Int): Int
}
