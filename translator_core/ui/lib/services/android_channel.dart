import 'dart:io' show Platform;

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

/// Android 采集通道（Dart ⇄ Kotlin）。
///
/// ## 职责边界
///
/// Dart 侧**不碰音频**：AudioRecord 在 Kotlin 侧，音频经
/// `translator_session_push_chunk` 直接进 Rust 会话队列。
/// 本通道只负责三件事：
///
/// 1. 传话 Rust 会话指针给 Kotlin（同一进程，同一份 .so）；
/// 2. 请求/撤销采集（启动前台服务、申请权限、投屏授权）；
/// 3. 读回采集状态与外部存储目录（模型放哪）。
///
/// ## 桌面端
///
/// [isAndroid] 为 false 时全部方法直接返回「不可用」，
/// 调用方无需到处写平台分支（与 Rust 侧平台抽象同一思路）。
class AndroidChannel {
  AndroidChannel._();

  static final AndroidChannel instance = AndroidChannel._();

  /// 通道名（与 MainActivity 的 CHANNEL 常量一致）
  static const String channelName = 'com.translator.app/capture';

  static const MethodChannel _ch = MethodChannel(channelName);

  /// 当前平台是否为 Android
  bool get isAndroid => Platform.isAndroid;

  /// 采集状态（mode / degrade / isZhuoYiTong）
  Future<CaptureStatus> captureStatus() async {
    if (!isAndroid) return const CaptureStatus.idle();
    try {
      final r = await _ch.invokeMapMethod<String, dynamic>('captureStatus');
      if (r == null) return const CaptureStatus.idle();
      return CaptureStatus(
        mode: r['mode'] as String? ?? 'idle',
        degrade: r['degrade'] as String? ?? '',
        isZhuoYiTong: r['isZhuoYiTong'] as bool? ?? false,
      );
    } on PlatformException catch (e) {
      debugPrint('[android] captureStatus 失败: ${e.message}');
      return const CaptureStatus.idle();
    }
  }

  /// 等待采集真正起来（轮询 [captureStatus]）。
  ///
  /// ## 为什么需要
  ///
  /// 投屏授权是**异步**的：`startCapture` 只是「已受理」，采集服务要等
  /// 用户在系统弹窗点「立即开始」才真正起来。启动后立刻查一次
  /// `captureStatus` 拿到的是**旧状态**（idle / 上一次的模式），
  /// 用户拒绝投屏时也就看不到真实的降级原因。
  ///
  /// 本方法轮询到 [mode] 变成非 idle（或超时）为止，返回最后一次状态。
  /// 调用方应**后台**调用（unawaited），不阻塞「转写已启动」的 UI 反馈。
  Future<CaptureStatus> waitForCaptureMode({
    Duration timeout = const Duration(seconds: 30),
    Duration interval = const Duration(milliseconds: 500),
  }) async {
    final deadline = DateTime.now().add(timeout);
    var last = const CaptureStatus.idle();
    while (DateTime.now().isBefore(deadline)) {
      last = await captureStatus();
      if (last.mode != 'idle') return last;
      await Future<void>.delayed(interval);
    }
    return last;
  }

  /// 启动采集（双路并行，2026-10-28）。
  ///
  /// [session] 是 Rust 会话指针（Dart 侧拥有，Kotlin 只借用）。
  ///
  /// [sysIndex] / [micIndex] 是**两路各自**的 Rust 采集器下标
  /// （由 `translator_session_capture_index` 按声源类型查得）：
  /// - sysIndex：系统内放路（0=系统内放），-1 = 不起
  /// - micIndex：麦克风路（1=麦克风），-1 = 不起
  ///
  /// 两路可同时为有效值 —— Android 14 允许 MediaProjection 内放
  /// 与麦克风并行。旧设计只传一个 sourceIndex，导致「默认走内放
  /// 时麦克风那路根本没采」，用户对着手机说话收音不到（2026-10-28
  /// 用户实测反馈）。
  ///
  /// [useSystemAudio] 为 true 时 Kotlin 会先弹 MediaProjection 授权；
  /// 用户拒绝 → 只起麦克风那路（内放路降级，由 [captureStatus] 如实回报）。
  ///
  /// 返回 null = 成功；非 null = 失败原因（可直接展示给用户）。
  Future<String?> startCapture({
    required int session,
    int sysIndex = -1,
    int micIndex = -1,
    bool useSystemAudio = false,
  }) async {
    if (!isAndroid) return '仅 Android 支持该采集通道';
    if (session == 0) return '会话指针无效';
    if (sysIndex < 0 && micIndex < 0) return '会话未创建任何采集器';
    try {
      await _ch.invokeMethod<int>('startCapture', {
        'session': session,
        'sysIndex': sysIndex,
        'micIndex': micIndex,
        'useSystemAudio': useSystemAudio,
      });
      return null;
    } on PlatformException catch (e) {
      debugPrint('[android] startCapture 失败: ${e.code} ${e.message}');
      return e.message ?? e.code;
    }
  }

  /// 停止采集。
  ///
  /// ⚠️ **必须 await** —— 回包前 Kotlin 已确保采集线程退出。
  /// Dart 紧接着释放 Rust 会话，顺序反了就是 use-after-free。
  Future<void> stopCapture() async {
    if (!isAndroid) return;
    try {
      await _ch.invokeMethod<int>('stopCapture');
    } on PlatformException catch (e) {
      debugPrint('[android] stopCapture 失败: ${e.message}');
    }
  }

  /// 运行时启停某一路采集（2026-10-28 用户需求「转写过程中实时切换采集源」）。
  ///
  /// [kind]：0 = 系统内放，1 = 麦克风（与 Kotlin 的 `KIND_*` 一致）。
  /// [enabled]：true = 启用该路，false = 停用该路。
  ///
  /// 返回 true = 该路已处于期望状态；false = 失败（如缺投屏授权）。
  ///
  /// ## 为什么 Android 需要单独一条通道
  ///
  /// Android 的音频由 Kotlin 的 `AudioCaptureService` 生产
  /// （AudioRecord → `sessionPushChunk` → Rust 队列），
  /// Rust 侧的采集器是 `ExternalPushCapture`（start/stop 皆空实现）。
  /// 所以真正要启停的是 **Kotlin 那边的 AudioRecord + 读线程**。
  ///
  /// 停掉的那一路，Rust 侧的接收端一直等不到数据 →
  /// 该路不产生任何字幕（不影响另一路）。
  Future<bool> setSourceEnabled(int kind, bool enabled) async {
    if (!isAndroid) return false;
    try {
      final ok = await _ch.invokeMethod<bool>('setCaptureSourceEnabled', {
        'kind': kind,
        'enabled': enabled,
      });
      return ok ?? false;
    } on PlatformException catch (e) {
      debugPrint('[android] setCaptureSourceEnabled 失败: ${e.code} ${e.message}');
      return false;
    }
  }

  /// 应用外部私有目录（模型放这里，adb push 直接可达）
  Future<String> externalFilesDir() async {
    if (!isAndroid) return '';
    try {
      final p = await _ch.invokeMethod<String>('externalFilesDir');
      return p ?? '';
    } on PlatformException catch (e) {
      debugPrint('[android] externalFilesDir 失败: ${e.message}');
      return '';
    }
  }

  /// 应用**内部**私有目录（/data/data/<pkg>/files）。
  ///
  /// ## 为什么模型要放这儿，而不是 externalFilesDir
  ///
  /// Android 11+ 分区存储下，`adb push` 推入
  /// `/storage/emulated/0/Android/data/<pkg>/files` 的文件**属主是 shell**，
  /// 权限 `drwxrws--- shell ext_data_rw` → 应用既不是 owner 也不在
  /// ext_data_rw 组 → 读不到：
  ///   "FileSystemException: Exists failed ... Permission denied, errno = 13"
  ///
  /// 内部私有目录权限天然正确（属主就是应用 UID），用
  /// `run-as <pkg> cp -r` 搬运即可，属主随之变成应用。
  Future<String> internalFilesDir() async {
    if (!isAndroid) return '';
    try {
      final p = await _ch.invokeMethod<String>('internalFilesDir');
      return p ?? '';
    } on PlatformException catch (e) {
      debugPrint('[android] internalFilesDir 失败: ${e.message}');
      return '';
    }
  }

  // ── 模型内置化（2026-10-28 任务 #17）──────────────────
  //
  // 模型打进 APK assets，首启由 Kotlin 解压到 filesDir/models。
  // Dart 侧流程：ensureModels（受理）→ 轮询 modelsStatus 直到
  // extracting=false 且 ready=true → 再探测模型目录建会话。
  // 已解压过的启动（标记命中）第一次轮询就返回 ready。

  /// 请求解压模型（幂等；已就位时 Kotlin 侧秒回 ready）。
  Future<void> ensureModels() async {
    if (!isAndroid) return;
    try {
      await _ch.invokeMethod<bool>('ensureModels');
    } on PlatformException catch (e) {
      debugPrint('[android] ensureModels 失败: ${e.message}');
    }
  }

  /// 查询解压状态。
  Future<ModelsStatus> modelsStatus() async {
    if (!isAndroid) return const ModelsStatus.ready();
    try {
      final r = await _ch.invokeMapMethod<String, dynamic>('modelsStatus');
      if (r == null) return const ModelsStatus.pending();
      return ModelsStatus(
        extracting: r['extracting'] as bool? ?? false,
        ready: r['ready'] as bool? ?? false,
        dir: r['dir'] as String? ?? '',
      );
    } on PlatformException catch (e) {
      debugPrint('[android] modelsStatus 失败: ${e.message}');
      return const ModelsStatus.pending();
    }
  }

  /// 阻塞等待模型就绪（bootstrap 用）。
  ///
  /// [onTick] 每次轮询回调（progress 为 null=进行中未知进度，
  /// 调用方可显示"正在准备模型"）。超时返回 false —— 超时不等于失败
  /// （解压可能因大文件偏慢），但 bootstrap 不能无限等，
  /// 超时后按「无模型」路径走 mock 兜底并在 UI 提示。
  Future<bool> waitForModels({
    Duration timeout = const Duration(minutes: 3),
    void Function(String hint)? onTick,
  }) async {
    if (!isAndroid) return true;
    await ensureModels();
    final deadline = DateTime.now().add(timeout);
    while (DateTime.now().isBefore(deadline)) {
      final st = await modelsStatus();
      if (st.ready && !st.extracting) return true;
      onTick?.call(st.extracting ? '正在安装模型，首次启动约需十几秒…' : '正在准备…');
      await Future.delayed(const Duration(milliseconds: 500));
    }
    debugPrint('[android] waitForModels 超时（$timeout）');
    return false;
  }

  // ── 悬浮字幕窗（2026-10-28 任务 #24）──────────────────
  //
  // Android 禁止第二实例进程，字幕窗改原生 Overlay
  // （SubtitleOverlayService）。SYSTEM_ALERT_WINDOW 不走运行时弹窗，
  // 必须跳系统设置页手动授予 —— 调用顺序：
  //   overlayPermission() 查询 → 未授予则 requestOverlayPermission()
  //   引导用户 → 用户回来后再次 overlayPermission() → showSubtitle()

  Future<bool> overlayPermission() async {
    if (!isAndroid) return true;
    try {
      return await _ch.invokeMethod<bool>('overlayPermission') ?? false;
    } on PlatformException catch (e) {
      debugPrint('[android] overlayPermission 失败: ${e.message}');
      return false;
    }
  }

  Future<void> requestOverlayPermission() async {
    if (!isAndroid) return;
    try {
      await _ch.invokeMethod<bool>('requestOverlayPermission');
    } on PlatformException catch (e) {
      debugPrint('[android] requestOverlayPermission 失败: ${e.message}');
    }
  }

  /// 打开字幕悬浮窗。返回 null=成功，非 null=失败原因。
  Future<String?> showSubtitle() async {
    if (!isAndroid) return '仅 Android 走原生字幕窗';
    try {
      await _ch.invokeMethod<bool>('showSubtitle');
      return null;
    } on PlatformException catch (e) {
      return e.message ?? e.code;
    }
  }

  Future<void> hideSubtitle() async {
    if (!isAndroid) return;
    try {
      await _ch.invokeMethod<bool>('hideSubtitle');
    } on PlatformException catch (e) {
      debugPrint('[android] hideSubtitle 失败: ${e.message}');
    }
  }

  /// 推送字幕内容（fire-and-forget，失败只记日志不阻塞转写）。
  ///
  /// 用户定版（2026-10-28）：字幕窗固定两行、满屏宽 ——
  /// 第 1 行原文、第 2 行译文；partial/partialZh 为流式实时行，
  /// 非空时各自顶替对应行。
  Future<void> updateSubtitle({
    required List<Map<String, String>> lines,
    String partial = '',
    String partialZh = '',
  }) async {
    if (!isAndroid) return;
    try {
      await _ch.invokeMethod<bool>('updateSubtitle', {
        'lines': lines,
        'partial': partial,
        'partialZh': partialZh,
      });
    } on PlatformException catch (e) {
      debugPrint('[android] updateSubtitle 失败: ${e.message}');
    }
  }

  /// 打开系统「另存为」对话框导出转写内容（SAF）。
  ///
  /// ## 为什么必须走 SAF 而不是自己写文件
  ///
  /// 用户要求（2026-10-28）：「导出不能导到软件目录，
  /// 要点击之后复制并打开目录选择」。
  ///
  /// Android 11+ 分区存储下，应用**无权**直接写 Downloads / 公共目录；
  /// 唯一正规途径是 Storage Access Framework 的 `ACTION_CREATE_DOCUMENT`：
  /// 系统弹「另存为」对话框 → 用户选目录 + 填文件名 → 系统回可写 URI。
  /// SAF 自带授权，**不需要任何存储权限**。
  ///
  /// ⚠️ 不能用 file_picker 的 saveFile（8.x 在 Android 上不支持，
  ///    会抛 UnimplementedError）—— 这正是旧实现退化成
  ///    「写应用私有目录（用户根本看不到）」的原因。
  ///
  /// 返回值：
  /// - 非空 = 导出成功的 URI（content://...）
  /// - 空串 = 用户取消或失败
  Future<String> exportSaveAs({
    required String content,
    required String fileName,
  }) async {
    if (!isAndroid) return '';
    try {
      final uri = await _ch.invokeMethod<String>('exportSaveAs', {
        'content': content,
        'fileName': fileName,
      });
      return uri ?? '';
    } on PlatformException catch (e) {
      debugPrint('[android] exportSaveAs 失败: ${e.message}');
      return '';
    }
  }

  /// 设备画像（平板判定由 Kotlin 权威给出）。
  ///
  /// ## 为什么不让 Dart 自己猜
  ///
  /// Android 侧的平板判定用 `smallestScreenWidthDp >= 600`
  /// （**设备**属性），而 Dart 侧能拿到的只有当前**窗口**尺寸。
  /// 两者在手机分屏 / 折叠屏折叠态会不一致 —— Dart 若按窗口短边猜，
  /// 会把分屏中的手机误判成平板，导航栏形态突变。
  ///
  /// 故这里直接向 Kotlin 要结果，**单一事实来源**。
  Future<DeviceProfile> deviceProfile() async {
    if (!isAndroid) return const DeviceProfile.phone();
    try {
      final r = await _ch.invokeMapMethod<String, dynamic>('deviceProfile');
      if (r == null) return const DeviceProfile.phone();
      return DeviceProfile(
        isTablet: r['isTablet'] as bool? ?? false,
        smallestWidthDp: (r['smallestWidthDp'] as num?)?.toInt() ?? 0,
        cores: (r['cores'] as num?)?.toInt() ?? 0,
        ramMb: (r['ramMb'] as num?)?.toInt() ?? 0,
        lowRam: r['lowRam'] as bool? ?? false,
      );
    } on PlatformException catch (e) {
      debugPrint('[android] deviceProfile 失败: ${e.message}');
      return const DeviceProfile.phone();
    }
  }
}

/// 模型解压状态（对应 Kotlin 的 `modelsStatus`）
class ModelsStatus {
  final bool extracting;
  final bool ready;
  final String dir;

  const ModelsStatus({
    required this.extracting,
    required this.ready,
    required this.dir,
  });

  const ModelsStatus.pending()
      : extracting = true,
        ready = false,
        dir = '';

  const ModelsStatus.ready()
      : extracting = false,
        ready = true,
        dir = '';
}

/// 设备画像（来自 Kotlin 的 OrientationPolicy）
class DeviceProfile {
  /// 是否平板（sw600dp 及以上的大屏设备）
  final bool isTablet;

  /// 设备最短边（dp）。折叠屏展开时会从 ~320 跳到 ~673。
  final int smallestWidthDp;

  /// CPU 核数（Runtime.availableProcessors，0=未知）
  final int cores;

  /// 物理内存 MB（0=未知）
  final int ramMb;

  /// 系统低内存设备标记（Go 版 Android）
  final bool lowRam;

  const DeviceProfile({
    required this.isTablet,
    required this.smallestWidthDp,
    this.cores = 0,
    this.ramMb = 0,
    this.lowRam = false,
  });

  const DeviceProfile.phone()
      : isTablet = false,
        smallestWidthDp = 0,
        cores = 0,
        ramMb = 0,
        lowRam = false;

  /// Android 默认 ASR 线程数（任务 #23 线程调整器）。
  ///
  /// 产品定位（MEMORY 2026-10-25）：**会议字幕跟手是核心价值**，
  /// 默认满档，不因"怕抢资源"主动降档；只有硬件真实受限才收：
  /// - lowRam / <4 核 → 2（保跟手的下限；1 线程在手机上 RTF 无余量）
  /// - 4~5 核 → 4
  /// - ≥6 核（旗舰/平板）→ 6（sherpa 实测再高收益递减）
  /// 桌面端的 8 线程档在 ARM 移动 SoC 上没有对应收益，不开放。
  int get defaultAsrThreads {
    if (cores == 0) return 4; // 画像缺失 → 保守默认
    if (lowRam || cores < 4) return 2;
    if (cores < 6) return 4;
    return 6;
  }

  @override
  String toString() =>
      'DeviceProfile(isTablet: $isTablet, sw: ${smallestWidthDp}dp, '
      'cores: $cores, ramMb: $ramMb, lowRam: $lowRam)';
}

/// 采集状态快照（对应 Kotlin 的 `captureStatus`）
class CaptureStatus {
  /// idle | systemAudio | microphone | dual
  final String mode;

  /// 降级原因（空 = 未降级）。**禁止静默降级**（DESIGN §2.5.2）：
  /// 非空时 UI 必须显式提示。
  final String degrade;

  /// 是否运行在卓易通容器内
  final bool isZhuoYiTong;

  const CaptureStatus({
    required this.mode,
    required this.degrade,
    required this.isZhuoYiTong,
  });

  const CaptureStatus.idle()
      : mode = 'idle',
        degrade = '',
        isZhuoYiTong = false;

  /// 是否正在采集。
  ///
  /// ⚠️ 必须包含 `dual`（2026-10-28 双路并行采集）：
  /// 双路时代内放+麦克风同时跑，模式为 "dual"。
  /// 漏判会让 UI 以为没在采集 → 误显示「未运行」。
  bool get running =>
      mode == 'systemAudio' || mode == 'microphone' || mode == 'dual';
  bool get degraded => degrade.isNotEmpty;
}
