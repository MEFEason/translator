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
import java.io.File
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
        val sysIndex: Int,
        val micIndex: Int,
        val reply: MethodChannel.Result,
    )

    private var pending: PendingStart? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        // ⚠️ 必须在 super.onCreate 之前定方向：否则首帧会按系统当前方向
        //    渲染出来，手机横屏用户会看到「转一下才立正」的闪动。
        OrientationPolicy.apply(this)

        // ★★ 必须在 super.onCreate()（= Flutter 引擎启动、Dart 开始执行）
        //    之前、于**主线程**加载原生库。理由见 preloadNativeLibraries()。
        preloadNativeLibraries()

        super.onCreate(savedInstanceState)
    }

    /**
     * 在主线程预加载原生库（2026-10-28 真机闪退修复）。
     *
     * ## 崩溃现象
     *
     * 接入 Bergamot（链接了 Marian/sentencepiece 等 C++ 静态库）后，
     * APP **一启动就闪退**：
     * ```
     * signal 11 (SIGSEGV), null pointer dereference
     *   #00 libtranslator_core.so
     *   #01 libtranslator_core.so
     *   #02 linker64 __dl__ZN6soinfo17call_constructorsEv+752  ← 静态构造器
     *   #03 linker64 do_dlopen
     *   #05 libdl.so dlopen
     *   #09 [anon:dart-code]                                     ← Dart isolate 线程
     * ```
     *
     * ## 根因
     *
     * 崩在 **dlopen 时刻的 C++ 静态初始化器**里（`soinfo::call_constructors`），
     * 而这次 dlopen 是由 **Dart 的 isolate 线程**发起的
     * （Dart 侧 `DynamicLibrary.open('libtranslator_core.so')`）。
     *
     * Android 的每个线程都有独立的 **TLS（线程局部存储）**。
     * Dart isolate 线程不是进程主线程，它的 TLS 尚未由运行时初始化，
     * 此时执行 C++ 全局对象的构造器（marian/spdlog 的 logger 等会访问
     * `thread_local`）就会解引用空指针 → SIGSEGV。
     *
     * 之前没暴露：Android 端此前只链 Rust 纯代码 + sherpa 的 .so，
     * **libtranslator_core.so 里没有任何 C++ 静态构造器**，
     * dlopen 阶段不会跑用户代码。
     *
     * ## 修法
     *
     * 在**主线程**（TLS 已正确初始化）且**Flutter 引擎启动之前**
     * 先 `System.loadLibrary` 一次。之后 Dart 侧再
     * `DynamicLibrary.open('libtranslator_core.so')` 时，动态链接器按
     * soname 找到**已加载的同一个库**（同一 inode）→ 直接返回已有 handle，
     * **不会重跑静态构造器** → 崩不了。
     *
     * （Dart 侧用的也是库名而非绝对路径，两边加载的是同一份。）
     */
    private fun preloadNativeLibraries() {
        for (name in arrayOf("translator_core", "native_bridge")) {
            try {
                System.loadLibrary(name)
                Log.i(TAG, "预加载 $name 成功（主线程）")
            } catch (e: UnsatisfiedLinkError) {
                // 不在这里崩 —— 让后续的 Dart 侧报错给出更完整的上下文
                Log.e(TAG, "预加载 $name 失败", e)
            }
        }
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
            // ⚠️ 2026-10-27：模型目录的**首选**落点 = 应用内部私有目录。
            //
            // 为什么不能只用 externalFilesDir（/storage/emulated/0/Android/data/<pkg>/files）：
            //   Android 11+ 分区存储下，`adb push` 推入的文件属主是 **shell**，
            //   权限位 `drwxrws--- shell ext_data_rw` → 应用（属主是 u0_xxx）
            //   **既不是 owner 也不在 ext_data_rw 组** → 访问被拒：
            //     "FileSystemException: Exists failed ... (OS Error: Permission denied, errno = 13)"
            //   内部私有目录（/data/data/<pkg>/files）权限天然正确，无需 chmod。
            //
            // 搬运方式（deploy 脚本里用）：`adb push` 到 /data/local/tmp，
            // 再 `run-as <pkg> cp -r` 进 files/ —— 属主即变成应用 UID。
            "internalFilesDir" -> reply.success(filesDir.absolutePath)
            // ── 模型内置化（2026-10-28 任务 #16/#17）──
            // ensureModels：后台线程解压 assets → filesDir/models。
            // ⚠️ 绝不能在 MethodChannel 主线程同步解压（441MB 要 5~15s，
            //    会卡死 UI 触发 ANR）—— 立即回「已受理」，
            //    Dart 侧经 modelsStatus 轮询直到 extracting=false。
            "ensureModels" -> {
                thread(name = "model-extract") {
                    val r = ModelExtractor.ensureModels(this)
                    Log.i(TAG, "模型解压结束：${r ?: "失败（详见 TranslatorModels 日志）"}")
                }
                reply.success(true)
            }
            "modelsStatus" -> {
                // ⚠️ ready 判定必须与 ModelExtractor.ensureModels 完全一致：
                // 标记**存在且内容等于当前 versionName**。
                // 第一版只查 exists() —— 重装升级后旧标记残留，
                // Dart 误判就绪、探到解压到一半的目录（2026-10-28 真机日志实锤：
                // "目录命中"比"解压完成"早 0.7s）。宁可多等一次轮询。
                val marker = File(filesDir, "models/.extracted")
                val currentVersion = try {
                    packageManager.getPackageInfo(packageName, 0).versionName ?: "unknown"
                } catch (e: Exception) { "unknown" }
                val ready = marker.exists() &&
                    runCatching { marker.readText().trim() == currentVersion }.getOrDefault(false)
                reply.success(
                    mapOf(
                        "extracting" to ModelExtractor.isExtracting(),
                        "ready" to ready,
                        "dir" to File(filesDir, "models").absolutePath,
                    )
                )
            }
            "deviceProfile" -> reply.success(OrientationPolicy.profileOf(this))
            // ── 悬浮字幕窗（2026-10-28 任务 #24）──
            "overlayPermission" -> reply.success(SubtitleOverlayService.hasPermission(this))
            "requestOverlayPermission" -> {
                SubtitleOverlayService.requestPermission(this)
                reply.success(true)
            }
            "showSubtitle" -> {
                if (!SubtitleOverlayService.hasPermission(this)) {
                    reply.error("NO_OVERLAY_PERMISSION", "未授予悬浮窗权限", null)
                    return
                }
                SubtitleOverlayService.start(this)
                reply.success(true)
            }
            "hideSubtitle" -> {
                SubtitleOverlayService.stop(this)
                reply.success(true)
            }
            "updateSubtitle" -> {
                @Suppress("UNCHECKED_CAST")
                val lines = (call.argument<List<Map<String, Any>>>("lines") ?: emptyList())
                    .map { (it["s"] as? String ?: "") to (it["t"] as? String ?: "") }
                val partialSrc = call.argument<String>("partial") ?: ""
                val partialZh = call.argument<String>("partialZh") ?: ""
                SubtitleOverlayService.instance
                    ?.updateSubtitle(lines, partialSrc, partialZh)
                reply.success(true)
            }
            "setCaptureGranted" -> {
                // Kotlin 是授权的实际感知方 → 把结果转给 Rust 会话，
                // 让 capture_capability 如实上报（禁止静默降级，DESIGN §2.5.2）
                val session = call.argument<Long>("session") ?: 0L
                val granted = call.argument<Boolean>("granted") ?: false
                reply.success(NativeBridge.sessionSetCaptureGranted(session, granted))
            }
            // ── 转写导出（2026-10-28 用户需求）──
            // 打开系统「另存为」对话框（SAF ACTION_CREATE_DOCUMENT）：
            // 用户自己选目录 + 填文件名 → 写到用户指定位置。
            //
            // ⚠️ 不能用 file_picker 的 saveFile（8.x 在 Android 上不支持），
            //    也不能写应用私有目录（用户在文件管理器里根本看不到）。
            // SAF 自带授权，**不需要任何存储权限**。
            //
            // 异步语义：**reply 挂起**，直到用户在系统对话框里
            // 选完/取消，才回包（成功 = URI 字符串，取消 = 空串）。
            // Dart 侧 await 会挂起 —— 这正是期望行为。
            "exportSaveAs" -> {
                val content = call.argument<String>("content") ?: ""
                val fileName = call.argument<String>("fileName") ?: "transcript.txt"
                if (content.isEmpty()) {
                    reply.success("")
                    return
                }
                ExportSaveAsActivity.start(this, content, fileName) { uri ->
                    reply.success(uri ?: "")
                }
            }
            else -> reply.notImplemented()
        }
    }

    // ── startCapture ─────────────────────────────────

    private fun startCapture(call: MethodCall, reply: MethodChannel.Result) {
        val useSystemAudio = call.argument<Boolean>("useSystemAudio") ?: false
        val session = (call.argument<Number>("session"))?.toLong() ?: 0L
        // 双路下标（2026-10-28）：-1 = 该路不起。兼容旧参数 sourceIndex：
        // 若 Dart 只传了 sourceIndex 且没传 sys/mic 下标，按旧语义
        // （单路，useSystemAudio 决定它是不是内放）映射。
        val sysIndex = (call.argument<Number>("sysIndex"))?.toInt()
            ?: if (call.argument<Number>("micIndex") == null &&
                (call.argument<Number>("sourceIndex")) != null && useSystemAudio
            ) (call.argument<Number>("sourceIndex"))!!.toInt() else -1
        val micIndex = (call.argument<Number>("micIndex"))?.toInt()
            ?: if (call.argument<Number>("sysIndex") == null &&
                (call.argument<Number>("sourceIndex")) != null && !useSystemAudio
            ) (call.argument<Number>("sourceIndex"))!!.toInt() else -1

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
            pending = PendingStart(useSystemAudio, session, sysIndex, micIndex, reply)
            requestPermissions(audioPermList(), REQ_PERMS)
            return
        }

        launchCapture(useSystemAudio, session, sysIndex, micIndex, reply)
    }

    /** 权限齐备后的真正启动分流。 */
    private fun launchCapture(
        useSystemAudio: Boolean,
        session: Long,
        sysIndex: Int,
        micIndex: Int,
        reply: MethodChannel.Result,
    ) {
        if (useSystemAudio && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            // 内放：MediaProjection 授权必须由 Activity 前台弹窗发起。
            // 暂存参数，授权结果在 ProjectionPermissionActivity 处理。
            ProjectionPermissionActivity.pending =
                PendingProjectionParams(useSystemAudio, session, sysIndex, micIndex)
            try {
                ProjectionPermissionActivity.request(this)
            } catch (e: Exception) {
                // 容器等受限环境连 createScreenCaptureIntent 都可能抛
                // → 只起麦克风路（双路时代内放失败不拖累麦克风），
                //   **必须回包**（否则 Dart 的 await 挂死）
                Log.w(TAG, "投屏授权流无法发起：${e.message}，仅起麦克风")
                AudioCaptureService.degradeReason = "投屏授权不可用：${e.message}"
                ProjectionPermissionActivity.pending = null
                AudioCaptureService.start(this, false, session, -1, micIndex)
                reply.success(0)
                return
            }
            // 授权结果异步：这里先回「已受理」。Dart 侧随后经
            // captureStatus 轮询 activeMode 决定继续/降级。
            reply.success(0)
        } else {
            // 不内放（或 API<29）：只起麦克风路。sysIndex 传 -1。
            AudioCaptureService.start(this, false, session, -1, micIndex)
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
        runOnUiThread {
            launchCapture(p.useSystemAudio, p.session, p.sysIndex, p.micIndex, p.reply)
        }
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
    //
    // 2026-10-28：旧 onProjectionResult / onProjectionFlowAbandoned
    // 已删除 —— 全仓库无调用点（ProjectionPermissionActivity 直接调
    // AudioCaptureService，不经 MainActivity），且双路改造后签名
    // 已变。保留死代码只会误导后续维护。

    override fun onDestroy() {
        channel?.setMethodCallHandler(null)
        channel = null
        super.onDestroy()
    }
}
