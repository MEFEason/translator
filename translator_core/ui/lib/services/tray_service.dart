// 托盘服务。
//
// ## 目标
//
// 字幕翻译类应用的典型使用方式是「让它在后台一直开着，字幕随时出现」。
// 用户关掉窗口时如果直接退出进程，转写就断了 —— 所以：
//   - 点窗口的 ✕ → **最小化到通知栏**（可关，见设置项），不退出
//   - 左键点托盘图标 → 显示/隐藏主窗口
//   - 右键托盘图标 → 菜单（见 [_buildMenu]）
//
// ## ⚠️ 图标必须用 ICO，不能用 PNG（踩过的坑）
//
// 微软官方文档明示 `NOTIFYICONDATAW.hIcon` 的类型是 **HICON**，
// 必须由 `LoadImage` / `LoadIconMetric` 从 **.ico 资源**加载。
// 早期版本给 tray_manager 传的是 base64 PNG data URI，
// 结果：API 全部调用成功、日志打「托盘已就绪」，
// 但通知栏里**完全不显示**（Flutter 社区同样有记录：
// Windows 用 png 虽然占了位置但完全不显示，必须改用 .ico）。
//
// tray_manager 0.8.0 起 Windows 同时支持 .png 与 .ico（见官方文档
// "Upgrading from 0.5.x" 一节），但 ICO 才是官方推荐格式。
// 本模块传**文件路径**让 nativeapi 走 `Image.fromFile`，
// 与 data URI 那条不稳的路径区分开。
//
// ## 为什么不用自己写的 Win32 版本
//
// 曾改为 dart:ffi 直调 Shell_NotifyIconW + 自建 message-only 窗口，
// 结果**一点托盘就崩**。根因：`Pointer.fromFunction` 生成的回调
// trampoline **绑定在创建它的 isolate 所在线程**上；在主 isolate 创建
// 指针、却在 `Isolate.spawn` 出来的另一个线程注册窗口类并调用，
// 调用约定不匹配 → 崩溃。
//
// 若要在 Dart 侧自己接托盘消息，唯一正确的做法是把窗口过程挂到
// **Flutter 主窗口**（`SetWindowLongPtr` 子类化）—— 那仍是主 isolate
// 的线程。但 window_manager 0.5.2 没有暴露原始窗口消息入口
// （WindowListener 只有 onWindowClose/Focus/Resize 等，已核实），
// 因此改用 tray_manager，让插件的 C++ 侧处理消息循环。
//
// 官方参考：
//   https://learn.microsoft.com/windows/win32/api/shellapi/ns-shellapi-notifyicondataw
//   https://learn.microsoft.com/windows/win32/shell/notification-area
//
// ## 关于 legacy API
//
// tray_manager 0.8.0 提供两套入口：
//   - `package:tray_manager/tray_manager.dart` → 新版 nativeapi 事件式 API
//     （需自建 TrayIcon / Menu / 事件订阅，样板代码多）
//   - `package:tray_manager/legacy.dart`        → 旧版 setIcon/setContextMenu API
//     （本文件所用，语义直观）
//
// legacy 被官方标记为 deprecated，但**当前版本功能完整**。本模块刻意
// 选用它：托盘是稳定需求，而新 API 仍在演进。
// 移除 legacy 时（届时需改用 nativeapi 的 TrayIcon 事件流），
// 只需替换本文件的 _createTrayIcon / _buildMenu 两处。

// ignore_for_file: deprecated_member_use_from_same_package

import 'dart:io';

import 'dart:ffi' hide Size;
import 'dart:ui' show Size;

import 'package:ffi/ffi.dart';
import 'package:flutter/foundation.dart';

import 'package:path/path.dart' as p;
import 'package:tray_manager/legacy.dart';
import 'package:win32/win32.dart' as win;
import 'package:win32/win32.dart' show WNDENUMPROC;
import 'package:window_manager/window_manager.dart';

/// 托盘菜单项动作。
enum TrayAction { show, toggleSubtitles, toggleTranscribe, quit }

/// 托盘服务。
///
/// 单例使用：[TrayService.instance]。
class TrayService with TrayListener {
  TrayService._();

  static final TrayService instance = TrayService._();

  /// 「打开主界面」回调。
  void Function()? onShowWindow;

  /// 「打开/关闭字幕」回调。
  void Function()? onToggleSubtitles;

  /// 「开始/停止转写」回调（点一下开始，再点一下停止）。
  void Function()? onToggleTranscribe;

  /// 「退出程序」回调（先停转写、冲刷转写盘，再退出）。
  void Function()? onQuit;

  /// 关闭窗口的**拦截回调**（UI 注册）。
  ///
  /// 2026-10-25 用户要求「参考大厂：让用户选择最小化还是直接关闭」。
  /// 注册后，点 ✕ 由 UI 决定（弹窗询问 / 直接最小化 / 直接退出），
  /// 托盘不再自行分流。为 null 时退回 `_minimizeToTray` 的老逻辑。
  void Function()? onCloseIntercept;

  /// 当前是否正在转写（用于菜单文案：开始 / 停止）。
  bool Function()? isTranscribing;

  /// 字幕窗口当前是否可见（用于菜单文案：打开 / 关闭字幕）。
  bool Function()? areSubtitlesVisible;

  /// 窗口是否已隐藏到托盘。
  bool _hiddenToTray = false;

  /// 缓存的主窗口句柄。
  ///
  /// ⚠️ 必须缓存：`windowManager.getId()` 要走一次 platform channel
  /// 往返，每次点托盘都等它会有**可感延迟**。启动时取一次，之后复用；
  /// 取不到（0）才重新枚举。
  int _cachedHwnd = 0;

  /// 点 ✕ 是否「最小化到通知栏」而非退出。
  ///
  /// 2026-10-25：默认 **false** —— 点 ✕ 走退出流程（先显示「正在保存」
  /// 遮罩，保存完成后退出），与托盘「退出程序」一致。
  /// 只有用户主动开启开关，点 ✕ 才改为缩到通知栏继续转写。
  bool _minimizeToTray = false;

  bool get hiddenToTray => _hiddenToTray;

  /// 点 ✕ 是否「最小化到通知栏」（false = 走退出流程）
  bool get minimizeToTray => _minimizeToTray;

  /// 初始化：创建托盘图标 + 拦截窗口关闭。
  ///
  /// [minimizeToTray] 默认 **false**（2026-10-25 用户要求）：
  /// 点 ✕ 走退出流程（保存后退出），与托盘「退出程序」一致。
  /// 传 true 则点 ✕ 缩到通知栏继续转写。
  Future<void> init({bool minimizeToTray = false}) async {
    if (!TrayManager.instance.isSupported) {
      debugPrint('[tray] 当前平台不支持系统托盘，跳过');
      return;
    }

    windowManager.addListener(TrayWindowListener());
    _minimizeToTray = minimizeToTray;
    // ⚠️ **始终拦截关闭**，不能按开关条件拦截：
    // `setPreventClose(false)` 时点 ✕ 会直接走 window_manager 的默认
    // 关闭路径，`onWindowClose` 根本不会被调用 —— 而用户要求点 ✕ 也能
    // 显示「正在保存」遮罩再退出（2026-10-25）。
    // 具体行为（退出 vs 最小化）由 `onWindowClose` 读 `_minimizeToTray` 分流。
    await windowManager.setPreventClose(true);

    TrayManager.instance.addListener(this);

    try {
      await _createTrayIcon();
      await _buildMenuOnce();
      debugPrint('[tray] 系统托盘已就绪');
    } catch (e) {
      debugPrint('[tray] 托盘初始化失败: $e');
      return;
    }
  }

  /// 创建托盘图标。
  ///
  /// ⚠️ **必须传 .ico 文件路径，不能传 PNG data URI** ——
  /// `NOTIFYICONDATAW.hIcon` 是 HICON 类型，PNG 塞进去
  /// 「占了位置但不显示」（详见文件头说明）。
  Future<void> _createTrayIcon() async {
    final exeDir = p.dirname(Platform.resolvedExecutable);
    final icoPath = p.join(exeDir, 'app_icon.ico');
    if (!File(icoPath).existsSync()) {
      throw StateError('找不到托盘图标文件: $icoPath');
    }
    await TrayManager.instance.setIcon(icoPath);
    await TrayManager.instance.setToolTip('Translator — 离线实时字幕翻译');
  }

  // ── 菜单项引用（建一次，之后只改 label）──
  //
  // ⚠️ **绝不能反复 setContextMenu**（踩过的坑，实测菜单点击全无响应）：
  // tray_manager 每次 setContextMenu 都会新建 NativeMenuBinding，
  // 并通过 `Timer.run(binding.dispose)` **延迟释放上一个 binding**。
  // 原生层此时可能仍持着旧菜单指针 → 点菜单项时
  // MenuItemClickedEvent 送不到 Dart（日志里连「菜单点击」都不打）。
  //
  // 官方文档明示：`label` / `toolTip` / `checked` / `disabled`
  // **赋值后立即生效**，只有增删项才需要再调 setContextMenu。
  // 所以这里建一次菜单、保存 MenuItem 引用，之后只改 label。
  MenuItem? _itemSubtitles;
  MenuItem? _itemTranscribe;

  /// 建一次右键菜单（初始化时调用）。
  Future<void> _buildMenuOnce() async {
    _itemSubtitles = MenuItem(
      key: 'toggle_subtitles',
      label: '关闭字幕',
    );
    _itemTranscribe = MenuItem(
      key: 'toggle_transcribe',
      label: '开始转写',
    );
    await TrayManager.instance.setContextMenu(Menu(
      items: [
        MenuItem(key: 'show', label: '打开主界面'),
        MenuItem.separator(),
        _itemSubtitles!,
        _itemTranscribe!,
        MenuItem.separator(),
        MenuItem(key: 'quit', label: '退出程序'),
      ],
    ));
    // 建完立即按当前状态校正一次文案
    _syncMenuLabels();
  }

  /// 只改文案，不重建菜单绑定。
  void _syncMenuLabels() {
    final transcribing = isTranscribing?.call() ?? false;
    final subtitlesOn = areSubtitlesVisible?.call() ?? true;
    _itemSubtitles?.label = subtitlesOn ? '关闭字幕' : '打开字幕';
    _itemTranscribe?.label = transcribing ? '停止转写' : '开始转写';
  }

  /// 句柄是否属于**本进程**的 Flutter 窗口（类名含 FLUTTER_RUNNER）。
  ///
  /// 必须同时校验进程 id：独立字幕窗也是 FLUTTER_RUNNER 窗口，
  /// 不校验会把字幕窗当成主窗口（见 `_enumWindowsProc` 的同类注释）。
  bool _isMyFlutterWindow(int hwnd) {
    if (hwnd == 0) return false;
    final pid = calloc<Uint32>();
    win.GetWindowThreadProcessId(hwnd, pid);
    final mine = pid.value == win.GetCurrentProcessId();
    calloc.free(pid);
    if (!mine) return false;
    final cls = calloc<Uint16>(256).cast<Utf16>();
    win.GetClassName(hwnd, cls, 256);
    final name = cls.toDartString();
    calloc.free(cls.cast<Uint16>());
    return name.contains('FLUTTER_RUNNER');
  }

  /// 取 Flutter 主窗口的原生句柄。
  ///
  /// `windowManager.getId()` 是官方入口（Windows 上就是窗口句柄）。
  /// 拿不到（返回 0）再按类名枚举 —— ⚠️ 不能用 `FindWindow` 按类名找：
  /// 实测 `FindWindow('FLUTTER_RUNNER_WIN32_WINDOW', null)` 返回 0
  /// （该窗口不是顶层可枚举类名匹配的那个），必须 EnumWindows 遍历。
  /// 供悬浮窗拖动取句柄（复用 _mainHwnd）。
  Future<int> _mainHwndForDrag() => _mainHwnd();

  Future<int> _mainHwnd() async {
    if (_cachedHwnd != 0) return _cachedHwnd;
    var h = await windowManager.getId();
    if (h != 0) {
      _cachedHwnd = h;
      return h;
    }

    // 兜底 A：**前台窗口**。点 ✕ 的瞬间主窗口必然是前台窗口，
    // 这比 EnumWindows 类名匹配可靠得多 —— 2026-10-25 用户实测
    // 「点关闭键关不掉」时，就是卡在 getId() 返回 0 之后
    // EnumWindows 也没找到（h == 0 → ShowWindow 根本没执行）。
    final fg = win.GetForegroundWindow();
    if (fg != 0 && _isMyFlutterWindow(fg)) {
      _cachedHwnd = fg;
      return fg;
    }

    // EnumWindows 遍历，找类名含 FLUTTER_RUNNER 的顶层窗口。
    // 回调必须用 Pointer.fromFunction 转成原生函数指针 ——
    // 且**只能在创建它的 isolate（即主 isolate）所在线程被调用**，
    // EnumWindows 是同步调用，正好在同一线程，安全。
    _enumFound.clear();
    win.EnumWindows(_enumWindowsProcPtr, 0);
    if (_enumFound.isEmpty) return 0;
    // 多个时取第一个可见的
    for (final f in _enumFound) {
      if (win.IsWindowVisible(f) != 0) {
        _cachedHwnd = f;
        return f;
      }
    }
    _cachedHwnd = _enumFound.first;
    return _enumFound.first;
  }

  /// 显示主窗口（从托盘恢复）。
  ///
  /// ⚠️ 不用 `windowManager.show()`：实测该调用在本项目窗口上
  /// **不生效**（点了托盘、状态也翻了，窗口就是不出现/不消失）。
  /// 改用 Win32 `ShowWindow(SW_SHOW/SW_RESTORE)` 直调，已实测有效。
  Future<void> showWindow() async {
    final h = await _mainHwnd();
    debugPrint('[tray] 显示窗口 hwnd=$h');
    if (h != 0) {
      win.ShowWindow(h, win.SW_RESTORE);
      win.ShowWindow(h, win.SW_SHOW);
      win.SetForegroundWindow(h);
    }
    _hiddenToTray = false;
  }

  /// 隐藏到托盘。
  ///
  /// ⚠️ 优先 Win32 `ShowWindow(SW_HIDE)` 直调（`windowManager.hide()`
  /// 在本项目窗口上实测不生效，见 showWindow 注释）。
  ///
  /// ## 2026-10-25 修复「主窗口按关闭键关不掉」（用户实测）
  ///
  /// 原实现：`h == 0` 时**什么都不做**，却无条件把 `_hiddenToTray = true`。
  /// 而 `setPreventClose(true)` 仍然拦着关闭 → 窗口既不让关也不隐藏，
  /// 点 ✕ 完全没反应。
  /// 现在三级兜底，且**验证真的隐藏成功**才更新状态：
  /// 1. Win32 ShowWindow + IsWindowVisible 验证
  /// 2. 失败再试 `windowManager.hide()`
  /// 3. 仍失败 → 撤掉关闭拦截并关闭窗口：用户点 ✕ 至少要有效果，
  ///    不能卡在"既不让关也不隐藏"的死局
  Future<void> hideToTray() async {
    final h = await _mainHwnd();
    debugPrint('[tray] 隐藏窗口 hwnd=$h');
    var hidden = false;
    if (h != 0) {
      win.ShowWindow(h, win.SW_HIDE);
      hidden = win.IsWindowVisible(h) == 0;
      debugPrint('[tray] Win32 隐藏结果 hidden=$hidden');
    }
    if (!hidden) {
      // 兜底 1：window_manager 官方 API
      try {
        await windowManager.hide();
        hidden = true;
        debugPrint('[tray] windowManager.hide 成功');
      } catch (e) {
        debugPrint('[tray] windowManager.hide 失败: $e');
      }
    }
    if (!hidden) {
      // 兜底 2：Win32 直接销毁窗口。
      //
      // 不用 window_manager 的 close/destroy —— 它们在本项目窗口上
      // **实测不生效**（与 show/hide 同一批结论）。DestroyWindow 是
      // 同步调用、窗口即刻消失，是唯一能保证「点 ✕ 一定有效果」的手段。
      debugPrint('[tray] 隐藏失败，走 Win32 DestroyWindow 保底 hwnd=$h');
      if (h != 0) {
        win.DestroyWindow(h);
        hidden = true;
      }
      // 撤掉拦截，防止残留的 preventClose 影响后续关闭行为
      await windowManager.setPreventClose(false);
    }
    // 只有真隐藏了才置 true（原来无条件置 true，会让状态与事实不符）
    _hiddenToTray = hidden;
  }

  /// 窗口关闭事件（配合 `setPreventClose(true)`）。
  ///
  /// 返回 true 表示「已自行处理」，不再退出进程。
  Future<bool> handleWindowClose() async {
    await hideToTray();
    return true;
  }

  /// **同步**处理窗口关闭 —— WM_CLOSE 回调专用。
  ///
  /// ⚠️ 全程只用 Win32 + 缓存，**不 await 任何 platform channel**
  /// （理由见 `TrayWindowListener.onWindowClose` 的注释）。
  ///
  /// 三级处理：
  /// 1. `ShowWindow(SW_HIDE)` + `IsWindowVisible` 验证 → 真隐藏
  /// 2. 失败 → `DestroyWindow` 同步销毁（window_manager 的
  ///    close/destroy 在本窗口上实测不生效）
  /// 3. 连句柄都拿不到 → 撤掉 `preventClose`，让默认关闭路径生效
  void handleWindowCloseSync() {
    _diagLog('关闭请求（同步路径）');
    final h = _mainHwndSync();
    _diagLog('句柄 h=$h');
    if (h == 0) {
      // 拿不到句柄：撤掉拦截，让 window_manager 走默认关闭
      _diagLog('句柄为 0，撤掉 preventClose');
      windowManager.setPreventClose(false);
      return;
    }
    win.ShowWindow(h, win.SW_HIDE);
    if (win.IsWindowVisible(h) == 0) {
      _diagLog('已隐藏到托盘');
      _hiddenToTray = true;
      return;
    }
    // 隐藏失败：直接销毁窗口（同步，即刻生效）
    _diagLog('隐藏失败，DestroyWindow 保底');
    final ok = win.DestroyWindow(h);
    _diagLog('DestroyWindow 返回 $ok');
    _hiddenToTray = false;
  }

  /// 同步取主窗口句柄（只用 Win32 + 缓存，不碰 platform channel）。
  int _mainHwndSync() {
    if (_cachedHwnd != 0) return _cachedHwnd;
    // 点 ✕ 的瞬间主窗口必然是前台窗口 —— 最可靠的来源
    final fg = win.GetForegroundWindow();
    if (fg != 0 && _isMyFlutterWindow(fg)) {
      _cachedHwnd = fg;
      return fg;
    }
    _enumFound.clear();
    win.EnumWindows(_enumWindowsProcPtr, 0);
    if (_enumFound.isNotEmpty) {
      for (final f in _enumFound) {
        if (win.IsWindowVisible(f) != 0) {
          _cachedHwnd = f;
          return f;
        }
      }
      _cachedHwnd = _enumFound.first;
      return _enumFound.first;
    }
    return 0;
  }

  /// 切换「最小化到托盘」开关（设置项调用）。
  ///
  /// ⚠️ **不再动 `setPreventClose`** —— 关闭拦截始终开启（原因见
  /// `init()` 里的注释）。开关语义由 `TrayWindowListener.onWindowClose`
  /// 读取 `_minimizeToTray` 后分流。
  Future<void> setMinimizeToTray(bool enabled) async {
    _minimizeToTray = enabled;
  }

  /// 供 UI 在状态变化后刷新托盘菜单文案（只改 label，不重建菜单）。
  Future<void> refreshMenu() async {
    _syncMenuLabels();
  }

  // ── TrayListener 回调 ──────────────────────────────

  @override
  void onTrayIconMouseDown() {}

  @override
  void onTrayIconMouseUp() {
    debugPrint('[tray] 左键点击，hidden=$_hiddenToTray');
    // 左键：显示/隐藏切换
    if (_hiddenToTray) {
      showWindow();
    } else {
      hideToTray();
    }
  }

  @override
  void onTrayIconRightMouseDown() {}

  @override
  void onTrayIconRightMouseUp() {
    // ⚠️ 官方文档要求：右键菜单要**手动弹**，不会自动弹出。
    //    见 tray_manager README 的 0.5.x 示例：
    //      void onTrayIconMouseDown() {
    //        trayManager.popUpContextMenu();   // 例如弹出菜单
    //      }
    // ⚠️ 这里**不能**调 _refreshMenu()：那会重建菜单绑定，
    //    与弹菜单互相干扰（实测菜单无响应）。
    //    菜单文案的刷新改由状态变化时主动触发
    //    （见 refreshMenu 的调用方：start/toggleSubtitles 之后）。
    debugPrint('[tray] 右键点击，弹出菜单');
    TrayManager.instance.popUpContextMenu();
  }

  @override
  void onTrayMenuItemClick(MenuItem menuItem) {
    debugPrint('[tray] 菜单点击: ${menuItem.key}');
    switch (menuItem.key) {
      case 'show':
        onShowWindow?.call();
      case 'toggle_subtitles':
        onToggleSubtitles?.call();
      case 'toggle_transcribe':
        onToggleTranscribe?.call();
      case 'quit':
        // 退出时序由 onQuit 回调全权负责（弹遮罩 → 关字幕 → 停转写 → 销毁 → 退出）。
        // ⚠️ 这里**不能**紧接着 exit(0)：否则遮罩来不及渲染、程序秒退，
        // 用户看到的就是「点了退出毫无反应然后瞬间消失」。
        // 仅当调用方未注册 onQuit 时兜底直接退出。
        if (onQuit != null) {
          onQuit!.call();
        } else {
          exit(0);
        }
      default:
        debugPrint('[tray] 未知菜单项: ${menuItem.key}');
    }
  }
}

// ─────────────────────────────────────────────────────────────
// 悬浮字幕窗（置顶 + 无边框 + 可拖动）
// ─────────────────────────────────────────────────────────────

/// WM_NCLBUTTONDOWN：非客户区鼠标按下（拖动窗口的标准手法）。
const int _wmNcLButtonDown = 0x00A1;

/// HTCAPTION：命中「标题栏」区域。
///
/// 无边框窗没有真标题栏，但只要往窗口发
/// `WM_NCLBUTTONDOWN + HTCAPTION`，Windows 就会进入「拖动窗口」模态 ——
/// 这是让无边框窗可拖动的标准做法，不需要自己算鼠标位移。
const int _htCaption = 2;

/// 进入 / 退出悬浮字幕模式。
///
/// 悬浮模式特征（都是用户明确要求的）：
/// - **置顶**：`setAlwaysOnTop(true)` —— 盖在游戏/视频/浏览器上面
/// - **无边框**：`setAsFrameless()` —— 去掉标题栏，只留字幕内容
/// - **不进任务栏**：`setSkipTaskbar(true)` —— 不占任务栏位置
/// - **小窗**：只放字幕，尺寸收窄
/// - **可拖动**：见 [startDragFloatingWindow]
///
/// 退出时逐项还原。任何一步失败只记日志，不让整个切换失败。
Future<void> setFloatingSubtitleMode(bool enabled) async {
  try {
    if (enabled) {
      await windowManager.setAsFrameless();
      await windowManager.setAlwaysOnTop(true);
      await windowManager.setSkipTaskbar(true);
      await windowManager.setSize(__FloatingSize);
      debugPrint('[tray] 已进入悬浮字幕模式（置顶/无边框/可拖动）');
    } else {
      await windowManager.setAlwaysOnTop(false);
      await windowManager.setSkipTaskbar(false);
      await windowManager.setTitleBarStyle(TitleBarStyle.normal);
      await windowManager.setSize(__NormalSize);
      debugPrint('[tray] 已退出悬浮字幕模式');
    }
  } catch (e) {
    debugPrint('[tray] 切换悬浮模式失败: $e');
  }
}

/// 悬浮窗尺寸（窄长条，只放字幕）。
const __FloatingSize = Size(520, 220);

/// 常规窗口尺寸（退出悬浮模式时还原）。
const __NormalSize = Size(1080, 720);

/// 让悬浮窗进入「拖动」模态。
///
/// 原理：给自己发 `WM_NCLBUTTONDOWN` + `HTCAPTION`。
/// Windows 收到后会自动进入窗口拖动模态（跟随鼠标、松手结束），
/// 不需要自己追踪鼠标位移 —— 这是无边框窗可拖动的标准手法。
Future<void> startDragFloatingWindow() async {
  final h = await TrayService.instance._mainHwndForDrag();
  if (h == 0) return;
  win.SendMessage(h, _wmNcLButtonDown, _htCaption, 0);
}

/// [EnumWindows] 的枚举结果（顶层静态，供上面的静态回调写）。
///
/// EnumWindows 是**同步**调用 —— 回调在同一线程内跑完才返回，
/// 所以用静态变量传结果不需要加锁，也不跨 isolate 共享。
final List<int> _enumFound = <int>[];

/// EnumWindows 的回调（**必须是顶层静态函数**）。
///
/// ⚠️ `Pointer.fromFunction` 只接受**静态**函数，闭包/tear-off 会
/// 编译报错（G4D552D9A：Closures and tear-offs are not supported
/// because they can capture context）。
int _enumWindowsProc(int hwnd, int lParam) {
  // ⚠️ 先按**当前进程**过滤：独立字幕窗也是 FLUTTER_RUNNER 窗口，
  // 不过滤会把字幕窗当成主窗口（实测托盘隐藏/拖动会作用到错误窗口）。
  final pid = calloc<Uint32>();
  win.GetWindowThreadProcessId(hwnd, pid);
  final mine = pid.value == win.GetCurrentProcessId();
  calloc.free(pid);
  if (!mine) return 1;
  final cls = calloc<Uint16>(256).cast<Utf16>();
  win.GetClassName(hwnd, cls, 256);
  final name = cls.toDartString();
  calloc.free(cls.cast<Uint16>());
  if (name.contains('FLUTTER_RUNNER')) {
    _enumFound.add(hwnd);
  }
  return 1; // WNDENUMPROC 返回 BOOL，非 0 = 继续枚举
}

/// 上述回调的原生函数指针。
final _enumWindowsProcPtr =
    Pointer.fromFunction<WNDENUMPROC>(_enumWindowsProc, 0);

/// 托盘诊断日志 —— 写到 `%TEMP%\translator_tray.log`，**release 下也可见**。
///
/// 2026-10-25：用户连续两次反馈「点关闭键关不掉」，而 release 构建下
/// `debugPrint` 完全不输出，问题处于不可观测状态，只能靠猜。
/// 现在把关键路径写进文件，用户可直接把该日志贴回来定位。
///
/// ⚠️ 诊断日志自身失败**绝不能**影响主流程，全包在 try 里。
void _diagLog(String msg) {
  try {
    final dir =
        Platform.environment['TEMP'] ?? Platform.environment['TMP'] ?? '.';
    File('$dir${p.separator}translator_tray.log').writeAsStringSync(
      '${DateTime.now().toIso8601String()} $msg\n',
      mode: FileMode.append,
    );
  } catch (_) {
    // 忽略：诊断通道故障不影响业务
  }
}

/// 主窗口的 WindowListener。
///
/// 职责：点 ✕ 的行为（与托盘菜单「退出程序」对齐）。
class TrayWindowListener extends WindowListener {
  @override
  void onWindowClose() {
    // ⚠️ 2026-10-25 用户明确要求：点 ✕ 与托盘「退出程序」**行为一致** ——
    // 先显示「正在保存」遮罩、等保存完成后再退出，而不是直接关掉窗口，
    // 也不是静默缩到通知栏。
    //
    // 此前的演进（都不对）：
    // 1. 静默最小化到托盘 → 用户要的是能退出
    // 2. 同步 Win32 隐藏 / DestroyWindow 保底 → 用户实测「直接关掉了，这不对」
    // 最终定为：与托盘退出走同一条完整流程。
    // UI 注册了关闭拦截（main.dart）：由它按用户的 closeAction 分流
    // （每次询问 / 最小化 / 退出）—— 2026-10-25「参考大厂」需求。
    final intercept = TrayService.instance.onCloseIntercept;
    if (intercept != null) {
      intercept.call();
      return;
    }
    // 未注册（异常路径）：退回本地分流
    final onQuit = TrayService.instance.onQuit;
    if (TrayService.instance.minimizeToTray) {
      // 开了「最小化到通知栏」：缩到通知栏继续转写（同步 Win32 路径，
      // 不在 WM_CLOSE 回调链里 await platform channel）
      TrayService.instance.handleWindowCloseSync();
      return;
    }
    if (onQuit != null) {
      onQuit.call();
      return;
    }
    // 未注册退出回调（异常路径）：退回同步隐藏，至少不让窗口卡死
    TrayService.instance.handleWindowCloseSync();
  }
}
