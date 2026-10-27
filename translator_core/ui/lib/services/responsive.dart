import 'dart:io' show Platform;

import 'package:flutter/material.dart';

import 'android_channel.dart';

/// 设备画像从 [android_channel.dart] 定义，这里转出，
/// 便于调用方只 import 本文件就能拿到设备画像类型。
export 'android_channel.dart' show DeviceProfile;

/// 屏幕适配工具层。
///
/// ## 为什么需要它
///
/// 本项目要同时服务三类形态：
///
/// | 形态 | 方向策略 | 导航形态 |
/// |---|---|---|
/// | 手机 | **锁竖屏**（字幕要贴屏幕底部，横屏没法看） | 底部导航栏 |
/// | 平板 | **可旋转**（横屏能铺开更宽的字幕区） | 侧边导航栏 |
/// | 桌面 | 由用户自己拖窗口 | 底部导航栏（保持既有观感） |
///
/// 这些判断散落在各页面里会很快失控（改一个断点要翻五个文件），
/// 所以全部收敛到本文件，页面只调用这里的静态方法。
///
/// ## 单一事实来源：Android 的 sw600dp
///
/// 平板判定**以 Kotlin 侧为准**（`OrientationPolicy.isTablet`，
/// 用 `smallestScreenWidthDp >= 600`）。理由：
///
/// - 方向策略本身就必须由 Android 决定（Manifest 的
///   `android:screenOrientation` 是静态的，没法按设备类型区分），
///   Dart 若用另一套口径，会出现「Android 当平板放开了旋转，
///   Dart 却按手机摆底部导航」这种割裂。
/// - `smallestScreenWidthDp` 是**设备**属性；手机分屏时窗口变窄，
///   但它不变 → 不会把分屏中的手机误判成平板。
///
/// 折叠屏展开时 sw 会从 ~320 跳到 ~673，[ResponsiveScope] 监听窗口
/// 尺寸变化并重新向 Kotlin 询问 → 自动从「手机策略」切成「平板策略」。
///
/// ## 断点依据
///
/// - **平板 = sw ≥ 600dp**。与 Android 系统的 `sw600dp` 资源限定符
///   语义一致（也是 `androidx.window` 官方判定大屏的口径）。
/// - **Material 3 窗口尺寸类**：compact < 600 / medium 600~839 /
///   expanded ≥ 840。
///
/// ## 桌面端为什么不参与
///
/// 桌面窗口可以随便拖成 1080×560，看起来「像平板」，但它是鼠标 +
/// 键盘 + 可调整窗口的交互模型，跟平板的触屏 + 固定机身完全不同。
/// 把桌面也切到侧边栏会改变已经验收过的桌面版观感，
/// 所以桌面恒按「非平板」处理。
class Responsive {
  const Responsive._();

  /// Material 3 窗口尺寸类断点（逻辑像素 dp）
  static const double compactBreak = 600;
  static const double mediumBreak = 840;

  /// 平板正文最大宽度（横屏更宽，给字幕更多行）
  static const double tabletMaxWidthLandscape = 960;
  static const double tabletMaxWidthPortrait = 840;

  /// 是否运行在移动端（Android / iOS）
  static bool get isMobile => Platform.isAndroid || Platform.isIOS;

  /// 当前窗口尺寸（逻辑像素）
  static Size screenSize(BuildContext context) => MediaQuery.sizeOf(context);

  /// 是否横屏
  static bool isLandscape(BuildContext context) {
    final s = screenSize(context);
    return s.width > s.height;
  }

  /// 是否竖屏
  static bool isPortrait(BuildContext context) => !isLandscape(context);

  /// 是否平板设备。
  ///
  /// 数据来自 [ResponsiveScope] 提供的 [DeviceProfile]（Kotlin 权威）。
  /// 拿到之前先用窗口短边兜底 —— 与 sw600dp 口径基本一致，
  /// 只是让首帧不必等一次 MethodChannel 往返。
  static bool isTablet(BuildContext context) {
    final profile = DeviceProfileScope.maybeOf(context);
    if (profile != null) return profile.isTablet;
    if (!isMobile) return false;
    return MediaQuery.sizeOf(context).shortestSide >= compactBreak;
  }

  /// 是否手机（移动端且非平板）
  static bool isPhone(BuildContext context) => isMobile && !isTablet(context);

  /// 导航形态：平板用侧边导航栏，手机与桌面用底部导航栏。
  static bool useSideNavigation(BuildContext context) => isTablet(context);

  /// Material 3 窗口尺寸类
  static WindowSizeClass windowSizeClass(BuildContext context) {
    final w = MediaQuery.sizeOf(context).width;
    if (w < compactBreak) return WindowSizeClass.compact;
    if (w < mediumBreak) return WindowSizeClass.medium;
    return WindowSizeClass.expanded;
  }

  /// 正文最大宽度（超出会居中留白）。
  ///
  /// 平板横屏 1280dp 宽时如果让卡片铺满整行，读起来眼睛要来回扫；
  /// 限宽后视觉重心稳定。
  static double contentMaxWidth(BuildContext context) {
    if (!isTablet(context)) return double.infinity;
    return isLandscape(context)
        ? tabletMaxWidthLandscape
        : tabletMaxWidthPortrait;
  }
}

/// Material 3 窗口尺寸类
enum WindowSizeClass { compact, medium, expanded }

/// 提供 [DeviceProfile] 的作用域。
///
/// ## 放在哪里
///
/// 放在 **MaterialApp 的 `builder:` 里**（MaterialApp 内部、Navigator 之外）。
/// 两个原因：
///
/// 1. 它要用 `MediaQuery.sizeOf` 监听窗口变化，而 MediaQuery 是
///    MaterialApp 建的 —— 放在 MaterialApp 外面会因找不到 MediaQuery
///    祖先而断言失败。
/// 2. `builder:` 包在 Navigator **之外** → push 出来的次级页面
///    （关于页、渠道说明页）也在作用域内。若改放 `home:`，只有主壳
///    能拿到设备画像，次级页会退回「按窗口短边猜」。
class ResponsiveScope extends StatefulWidget {
  final Widget child;

  const ResponsiveScope({super.key, required this.child});

  @override
  State<ResponsiveScope> createState() => _ResponsiveScopeState();
}

class _ResponsiveScopeState extends State<ResponsiveScope> {
  DeviceProfile _profile = const DeviceProfile.phone();

  /// 上一次看到的**窗口短边**（dp）。
  ///
  /// 只盯短边而非整个 size：旋转时宽高互换但短边不变，而
  /// `smallestScreenWidthDp` 也只随**折叠屏展开 / 分屏**变。
  /// 用短边做触发条件 → 旋转不会白发一次 MethodChannel。
  double? _lastShortestSide;
  bool _loading = false;

  @override
  void initState() {
    super.initState();
    _loadProfile();
  }

  Future<void> _loadProfile() async {
    if (_loading) return;
    _loading = true;
    try {
      final p = await AndroidChannel.instance.deviceProfile();
      if (!mounted) return;
      setState(() => _profile = p);
    } finally {
      _loading = false;
    }
  }

  @override
  Widget build(BuildContext context) {
    final size = MediaQuery.sizeOf(context);
    final shortest = size.shortestSide;
    if (_lastShortestSide != shortest) {
      final first = _lastShortestSide == null;
      _lastShortestSide = shortest;
      if (!first) {
        // 窗口形态变了（折叠屏展开 / 分屏）→ 重新判定平板身份。
        // 放到帧后执行：不能在 build 期间 setState。
        WidgetsBinding.instance.addPostFrameCallback((_) => _loadProfile());
      }
    }
    return DeviceProfileScope(profile: _profile, child: widget.child);
  }
}

/// 下发 [DeviceProfile] 的 InheritedWidget。
///
/// 页面通过 `DeviceProfileScope.of(context)` 拿画像 —— 这会建立依赖，
/// 画像更新时相关 widget 自动重建。
class DeviceProfileScope extends InheritedWidget {
  final DeviceProfile profile;

  const DeviceProfileScope({
    super.key,
    required this.profile,
    required super.child,
  });

  static DeviceProfile? maybeOf(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<DeviceProfileScope>()?.profile;

  static DeviceProfile of(BuildContext context) =>
      maybeOf(context) ?? const DeviceProfile.phone();

  @override
  bool updateShouldNotify(DeviceProfileScope oldWidget) =>
      oldWidget.profile.isTablet != profile.isTablet ||
      oldWidget.profile.smallestWidthDp != profile.smallestWidthDp;
}

/// 把正文限制在平板的舒适阅读宽度内并居中。
///
/// 用在**列表页**（ListView 为 child）。
///
/// ## 约束传递为什么是安全的
///
/// `Align` 内部对子节点调 `constraints.loosen()`（见 Flutter SDK
/// `rendering/shifted_box.dart` 的 `RenderPositionedBox.performLayout`）。
/// `loosen()` **只把 minWidth/minHeight 归零，max 保持原值** ——
/// 所以 ListView 拿到的 maxHeight 仍是有界的，不会触发
/// "Vertical viewport was given unbounded height"。
///
/// 用 `Alignment.topCenter` 而非 `Center`：子节点比容器矮时贴顶对齐，
/// 不会在垂直方向漂到中间（列表页那样看着别扭）。
class ResponsiveBody extends StatelessWidget {
  final Widget child;

  /// 额外的垂直内边距（列表页通常由 ListView 自己的 padding 管）
  final double verticalPadding;

  const ResponsiveBody({
    super.key,
    required this.child,
    this.verticalPadding = 0,
  });

  @override
  Widget build(BuildContext context) {
    final maxWidth = Responsive.contentMaxWidth(context);
    if (maxWidth == double.infinity && verticalPadding <= 0) return child;
    return Padding(
      padding: EdgeInsets.only(bottom: verticalPadding),
      child: Align(
        alignment: Alignment.topCenter,
        child: ConstrainedBox(
          constraints: BoxConstraints(maxWidth: maxWidth),
          child: child,
        ),
      ),
    );
  }
}
