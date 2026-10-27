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

  /// 启动采集。
  ///
  /// [session] 是 Rust 会话指针（Dart 侧拥有，Kotlin 只借用）。
  /// [sourceIndex] 对应会话的声源下标（多声源时按序对应）。
  /// [useSystemAudio] 为 true 时 Kotlin 会先弹 MediaProjection 授权；
  /// 用户拒绝 → 自动降级麦克风（并由 [captureStatus] 如实回报）。
  ///
  /// 返回 null = 成功；非 null = 失败原因（可直接展示给用户）。
  Future<String?> startCapture({
    required int session,
    int sourceIndex = 0,
    bool useSystemAudio = false,
  }) async {
    if (!isAndroid) return '仅 Android 支持该采集通道';
    if (session == 0) return '会话指针无效';
    try {
      await _ch.invokeMethod<int>('startCapture', {
        'session': session,
        'sourceIndex': sourceIndex,
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
      );
    } on PlatformException catch (e) {
      debugPrint('[android] deviceProfile 失败: ${e.message}');
      return const DeviceProfile.phone();
    }
  }
}

/// 设备画像（来自 Kotlin 的 OrientationPolicy）
class DeviceProfile {
  /// 是否平板（sw600dp 及以上的大屏设备）
  final bool isTablet;

  /// 设备最短边（dp）。折叠屏展开时会从 ~320 跳到 ~673。
  final int smallestWidthDp;

  const DeviceProfile({
    required this.isTablet,
    required this.smallestWidthDp,
  });

  const DeviceProfile.phone()
      : isTablet = false,
        smallestWidthDp = 0;

  @override
  String toString() =>
      'DeviceProfile(isTablet: $isTablet, smallestWidthDp: $smallestWidthDp)';
}

/// 采集状态快照（对应 Kotlin 的 `captureStatus`）
class CaptureStatus {
  /// idle | systemAudio | microphone
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

  bool get running => mode == 'systemAudio' || mode == 'microphone';
  bool get degraded => degrade.isNotEmpty;
}
