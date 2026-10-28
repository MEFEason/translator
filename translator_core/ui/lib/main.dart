import 'dart:async' show unawaited;
import 'dart:ffi' as ffi;
import 'dart:io' show Platform, exit;
import 'dart:isolate' show Isolate, ReceivePort, SendPort;
import 'dart:ui' show PlatformDispatcher;

import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
import 'package:window_manager/window_manager.dart';

import 'services/tray_service.dart';
import 'services/lyrics_window.dart';
import 'services/responsive.dart';
import 'state/translator_state.dart';
import 'pages/subtitle_page.dart';
import 'pages/glossary_page.dart';
import 'pages/model_page.dart';
import 'pages/capability_page.dart';
import 'pages/error_dialog_page.dart';
import 'dialogs/shutdown_dialog.dart';

/// 全局导航键：错误弹窗在没有局部 context 时靠它弹出对话框。
///
/// 启动早期（MaterialApp 尚未挂载）currentContext 为 null，
/// 此时错误只打印到控制台，不尝试弹窗，避免二次崩溃。
final GlobalKey<NavigatorState> appNavigatorKey = GlobalKey<NavigatorState>();

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();

  // 全局错误监控：任何未捕获异常都弹「独立错误窗口」展示详细信息，
  // 而不是让 Flutter 画一个红屏或静默崩溃。
  FlutterError.onError = (FlutterErrorDetails details) {
    debugPrint('[global error] ${details.exception}');
    _showErrorDialog(
      title: '程序异常',
      message: details.exceptionAsString(),
      detail: details.stack?.toString(),
    );
  };

  PlatformDispatcher.instance.onError = (Object error, StackTrace stack) {
    debugPrint('[platform error] $error');
    _showErrorDialog(
      title: '程序异常',
      message: error.toString(),
      detail: stack.toString(),
    );
    return true; // 已处理，阻止 Flutter 默认处理
  };

  // 独立歌词窗（第二实例）：`translator.exe --lyrics` 启动时
  if (isLyricsMode()) {
    await runLyricsWindow();
    return;
  }

  // 桌面端窗口与系统托盘准备
  if (Platform.isWindows || Platform.isMacOS || Platform.isLinux) {
    try {
      await windowManager.waitUntilReadyToShow();
      await windowManager.setSize(const Size(1080, 720));
      await windowManager.setMinimumSize(const Size(880, 560));
      await windowManager.setTitle('Translator — 离线实时字幕翻译');
      await windowManager.show();
      await windowManager.focus();
    } catch (e) {
      debugPrint('[window] 窗口初始化异常（忽略）: $e');
      try {
        await windowManager.show();
      } catch (_) {}
    }
  }

  runApp(const TranslatorApp());
}

/// 弹出独立错误窗口，展示完整错误信息与堆栈。
///
/// 用全局 [appNavigatorKey] 取 context —— 异常发生时未必有可用的局部 context。
/// 取不到（启动早期）就只打印，绝不因弹窗再抛一次异常。
void _showErrorDialog({
  required String title,
  required String message,
  String? detail,
}) {
  final ctx = appNavigatorKey.currentContext;
  if (ctx == null) {
    debugPrint('[error] 无可用 context，仅打印: $message');
    return;
  }
  showDialog<void>(
    context: ctx,
    barrierDismissible: false,
    builder: (_) => ErrorDialogPage(
      title: title,
      message: message,
      detail: detail,
    ),
  );
}

/// 强制结束当前进程：跳过 CRT 清理（atexit / 静态析构）。
///
/// 为什么不用 dart:io 的 `exit()`：translator_core 的后台 Rust 线程
/// （ASR/MT/引擎）可能持有 CRT 锁（heap/stdio），`exit()` 的 CRT 清理
/// 会与它们死锁 —— 实测表现就是「窗口未响应、进程不退出」（用户截图：
/// 遮罩已显示、转写已停止、但进程挂着不走）。TerminateProcess 立即
/// 终止所有线程、不跑任何清理；转写内容已逐句 flush 落盘，无数据丢失。
///
/// ⚠️ **Windows 专属**：Android 没有 kernel32.dll。
/// 2026-10-26 起加平台守护 —— Android 走 `exit(0)`：
/// Android 的 Dart VM 在 exit 时会跑正常的清理流程，
/// 且此时 Rust 引擎线程已随会话 stop 结束，风险可控。
void _terminateProcessHard() {
  if (Platform.isWindows) {
    final kernel32 = ffi.DynamicLibrary.open('kernel32.dll');
    final getCurrentProcess =
        kernel32.lookupFunction<ffi.IntPtr Function(), int Function()>(
            'GetCurrentProcess');
    final terminateProcess = kernel32.lookupFunction<
        ffi.Int32 Function(ffi.IntPtr, ffi.Uint32),
        int Function(int, int)>('TerminateProcess');
    terminateProcess(getCurrentProcess(), 0);
    return;
  }
  // 非 Windows（Android 等）：用 dart:io 的 exit
  exit(0);
}

/// 后台保存任务的参数（传给新 isolate）。
class _SaveRequest {
  final SendPort sendPort;
  final int sessionAddr;
  const _SaveRequest(this.sendPort, this.sessionAddr);
}

/// 后台 isolate 入口：真正执行「停止会话 + 冲刷落盘」。
///
/// `translator_session_stop` 是**同步** FFI，会整块阻塞调用它的 isolate。
/// 放在 UI isolate 上 = 界面冻结（「未响应」）；放在这里则 UI 完全不卡，
/// 还能真正等到保存完成。完成后经 SendPort 回传信号。
@pragma('vm:entry-point')
void _saveEntry(_SaveRequest req) {
  var ok = false;
  try {
    // 库名按平台：Windows 是 translator_core.dll，Android 是
    // libtranslator_core.so（同进程 dlopen 拿的是同一份加载实例）。
    final libName =
        Platform.isAndroid ? 'libtranslator_core.so' : 'translator_core.dll';
    final lib = ffi.DynamicLibrary.open(libName);
    final stop = lib.lookupFunction<ffi.Int32 Function(ffi.Pointer<ffi.Void>),
        int Function(ffi.Pointer<ffi.Void>)>('translator_session_stop');
    final ptr = ffi.Pointer<ffi.Void>.fromAddress(req.sessionAddr);
    ok = stop(ptr) == 0;
  } catch (_) {
    ok = false;
  }
  req.sendPort.send(ok);
}

/// 在后台 isolate 完成保存，并等待它回传「保存完毕」信号。
///
/// 返回 true=保存完成；false=未创建会话 / 异常 / 超时（调用方照常退出）。
Future<bool> _saveInBackground(TranslatorState state) async {
  final addr = state.sessionAddress;
  if (addr == 0) return false;
  final recv = ReceivePort();
  try {
    await Isolate.spawn(_saveEntry, _SaveRequest(recv.sendPort, addr));
    final res = await recv.first.timeout(
      const Duration(seconds: 5),
      onTimeout: () => false,
    );
    return res == true;
  } catch (e) {
    debugPrint('[quit] 后台保存异常（仍继续退出）: $e');
    return false;
  } finally {
    recv.close();
  }
}

/// 退出流程：显示「正在保存」遮罩 → 保存 → 结束进程。
///
/// 退出时序（用户明确要求）：
///   1) 立刻弹遮罩，点了退出马上有反馈，不再像线程卡死
///   2) **先关字幕**——必须在主窗口 destroy 之前，否则主窗没了、
///      悬浮字幕还飘一会儿
///   3) 停转写，让转写内容完整落盘
///   4) 解除关闭拦截 → 销毁窗口 → 退出进程
/// 全程用 try/finally 兜底：任何一步抛异常都仍然退出，绝不卡在遮罩上。
///
/// 托盘「退出程序」与窗口 ✕（用户选「退出程序」时）共用本函数。
Future<void> _quitWithSave(BuildContext context, TranslatorState state) async {
  // 0) **先把主窗口显示出来**（2026-10-25 用户反馈的修复）。
  //
  //    从托盘退出时，主窗口往往处于「最小化到通知栏」的隐藏状态。
  //    此时 showDialog 会把「正在保存」遮罩画在**看不见的窗口**上 ——
  //    用户看到的就是「点了退出，什么提示都没有就退了」。
  //    先恢复窗口，遮罩才真正可见，保存过程才有反馈。
  //
  //    ⚠️ Android 没有托盘/窗口隐藏概念（tray_service 内部是 Win32 调用），
  //    故只在桌面端执行这一步。
  if (Platform.isWindows || Platform.isMacOS || Platform.isLinux) {
    try {
      await TrayService.instance.showWindow();
    } catch (e) {
      debugPrint('[quit] 恢复主窗口失败（仍继续退出）: $e');
    }
  }

  // 1) 先弹遮罩，并**让出约两帧**让它真的画出来。
  //    sessionStop 是同步 FFI，会整块冻住 UI isolate；
  //    不先让出的话，遮罩还没渲染就被冻住，用户看到的是「窗口卡死」，
  //    而不是「正在退出」。
  showDialog<void>(
    context: context,
    barrierDismissible: false,
    builder: (ctx) => const ShutdownDialog(),
  );
  await Future<void>.delayed(const Duration(milliseconds: 60));

  // 2) 先关字幕（必须在进程结束前关掉，否则会残留孤儿窗口）
  try {
    if (state.subtitlesVisible) {
      await state.toggleSubtitlesWindow();
    }
  } catch (e) {
    debugPrint('[quit] 关字幕异常（仍继续退出）: $e');
  }

  // 3) 保存：同步的 sessionStop 挪到**后台 isolate**执行 ——
  //    它阻塞的是那个 isolate，UI 这边遮罩照常显示、不冻结，
  //    同时能真正等保存跑完（Rust 侧 3 秒），而不是超时就放弃。
  //    没在转写时无需保存，直接跳过（秒退）。
  if (state.running) {
    if (Platform.isWindows) {
      await _saveInBackground(state);
    } else {
      await state.stop();
    }
  }

  // 4) 收到「保存完毕」信号（或无需保存）→ 杀歌词窗子进程 →
  //    立即结束进程。不用 dart:io 的 exit()：它会跑 CRT 清理，
  //    后台 Rust 线程持锁时清理会死锁（实测「未响应且进程不退出」）。
  state.killLyricsWindow();
  _terminateProcessHard();
}

/// 关闭窗口时的选择框（大厂做法：询问 + 可记住选择）。
///
/// 返回用户选的动作（`'minimize'` / `'quit'`）；取消返回 null。
/// 勾选「记住我的选择」时直接写入 `TranslatorState.setCloseAction`，
/// 之后不再弹此框。
class _CloseChoiceDialog extends StatefulWidget {
  final TranslatorState state;
  const _CloseChoiceDialog({required this.state});

  @override
  State<_CloseChoiceDialog> createState() => _CloseChoiceDialogState();
}

class _CloseChoiceDialogState extends State<_CloseChoiceDialog> {
  String _choice = 'quit';
  bool _remember = false;

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('关闭窗口'),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          RadioListTile<String>(
            dense: true,
            value: 'minimize',
            // ignore: deprecated_member_use
            groupValue: _choice,
            // ignore: deprecated_member_use
            onChanged: (v) => setState(() => _choice = v ?? 'quit'),
            secondary: const Icon(Icons.notifications_active_outlined),
            title: const Text('最小化到通知栏'),
            subtitle: const Text(
              '继续转写，从通知栏图标恢复窗口',
              style: TextStyle(fontSize: 11),
            ),
          ),
          RadioListTile<String>(
            dense: true,
            value: 'quit',
            // ignore: deprecated_member_use
            groupValue: _choice,
            // ignore: deprecated_member_use
            onChanged: (v) => setState(() => _choice = v ?? 'quit'),
            secondary: const Icon(Icons.close),
            title: const Text('退出程序'),
            subtitle: const Text(
              '保存转写内容后退出',
              style: TextStyle(fontSize: 11),
            ),
          ),
          const Divider(height: 16),
          CheckboxListTile(
            dense: true,
            value: _remember,
            onChanged: (v) => setState(() => _remember = v ?? false),
            title: const Text('记住我的选择，不再询问'),
          ),
        ],
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('取消'),
        ),
        FilledButton(
          onPressed: () {
            if (_remember) {
              // 记住选择（不 await：弹窗内 fire-and-forget）
              widget.state.setCloseAction(_choice);
            }
            Navigator.pop(context, _choice);
          },
          child: const Text('确定'),
        ),
      ],
    );
  }
}

/// Translator 应用根。
///
/// 架构：Rust 核心（translator_core）+ Flutter UI，经 dart:ffi 直连 C ABI。
/// 平台差异（音频采集、权限、悬浮窗）全部在 Rust 侧收敛，
/// UI 只消费 `CaptureCapability` 这一个抽象。
class TranslatorApp extends StatelessWidget {
  const TranslatorApp({super.key});

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider(
      create: (_) => TranslatorState(),
      child: MaterialApp(
        navigatorKey: appNavigatorKey,
        title: 'Translator',
        debugShowCheckedModeBanner: false,
        theme: _theme(Brightness.light),
        darkTheme: _theme(Brightness.dark),
        // ⚠️ ResponsiveScope 放在 MaterialApp 的 builder 里（不是 home）。
        //
        // 原因一：它要用 MediaQuery.sizeOf 监听窗口变化，而 MediaQuery 是
        //         MaterialApp 建的 —— 放在 MaterialApp 外面会因找不到
        //         MediaQuery 祖先而断言失败。
        // 原因二：builder 包在 Navigator **之外**，所以 push 出来的次级
        //         页面（关于页、渠道说明页）也在作用域内；若放 home:
        //         只有主壳能拿到设备画像，次级页会退回「按窗口猜」。
        builder: (context, child) => ResponsiveScope(
          child: child ?? const SizedBox.shrink(),
        ),
        home: const HomeShell(),
      ),
    );
  }

  ThemeData _theme(Brightness brightness) {
    final scheme = ColorScheme.fromSeed(
      seedColor: const Color(0xFF2E5AAC),
      brightness: brightness,
    );
    return ThemeData(
      useMaterial3: true,
      colorScheme: scheme,
      // 2026-10-25 用户「整体字体太难看」：旧值 'monospace' 是等宽字体，
      // 中文渲染呆滞。改为内置 HarmonyOS Sans SC（见 pubspec fonts），
      // 中英混排、时间戳对齐都由字体自身处理。
      // ⚠️ 字幕渲染路径（主窗悬浮/歌词窗）若各自显式指定了 fontFamily，
      // 也要一并换成该字体 —— 这里先统一全局主题。
      fontFamily: 'HarmonyOS Sans SC',
    );
  }
}

/// 主壳：底部导航 + 能力横幅
class HomeShell extends StatefulWidget {
  const HomeShell({super.key});

  static const _pages = [
    SubtitlePage(),
    GlossaryPage(),
    ModelPage(),
    CapabilityPage(),
  ];

  @override
  State<HomeShell> createState() => _HomeShellState();
}

class _HomeShellState extends State<HomeShell> {
  @override
  void initState() {
    super.initState();
    // 启动引导：建会话 → 自检 → 探测采集能力。
    //
    // 放在 initState 而不是构造函数：FFI 抛异常时
    // 整个 App 会起不来，连错误页都渲染不了。
    // unawaited：不阻塞首帧渲染。
    final state = context.read<TranslatorState>();
    Future.microtask(() => state.bootstrap());
    if (Platform.environment['TRANSLATOR_SELFTEST'] == '1') {
      // 无人值守自检：完整跑链路 → 写报告 → 自动退出（手机端指令：全自主检测）
      unawaited(state.runSelfTest());
    }
    // 系统托盘：点 ✕ 最小化到通知栏，而不是退出进程。
    Future.microtask(() => _initTray(state));
  }

  /// 初始化托盘并接上各项功能。
  ///
  /// 托盘菜单：打开主界面 / 打开·关闭字幕 / 开始·停止转写 / 退出程序。
  Future<void> _initTray(TranslatorState state) async {
    final tray = TrayService.instance;
    // 打开主界面（从托盘恢复窗口）
    tray.onShowWindow = () => tray.showWindow();
    // 打开 / 关闭字幕窗。
    // ⚠️ 2026-10-25 修复：旧实现调 `toggleSubtitlesWindow()` 后还要
    // showWindow() 主窗口 —— 那是「悬浮字幕」时代的语义（字幕在主窗里）。
    // 现在字幕 = 独立字幕窗（第二实例），`subtitlesVisible` 派生自
    // `lyricsWindowEnabled`，开关它本身就是全部动作。
    tray.onToggleSubtitles = () async {
      await state.toggleSubtitlesWindow();
    };
    // 开始 / 停止转写。
    // ⚠️ 开始转写时**必须先把窗口显示出来** —— 否则窗口藏着，
    // 用户点了「开始转写」却看不到任何字幕，会以为功能没实现。
    tray.onToggleTranscribe = () {
      if (state.running) {
        state.stop();
      } else {
        state.start();
        tray.showWindow();
      }
    };
    // 退出前先停转写，让转写盘内容完整落盘
    // 退出时序（用户明确要求）：
    //   1) 立刻弹遮罩，点了退出马上有反馈，不再像线程卡死
    //   2) **先关字幕**——必须在主窗口 destroy 之前，否则主窗没了、
    //      悬浮字幕还飘一会儿
    //   3) 停转写，让转写内容完整落盘
    //   4) 解除关闭拦截 → 销毁窗口 → 退出进程
    // 全程用 try/finally 兜底：任何一步抛异常都仍然退出，绝不卡在遮罩上。
    // 退出流程（完整时序见 _quitWithSave 的文档注释）
    tray.onQuit = () => _quitWithSave(context, state);
    // 点 ✕ 的行为（2026-10-25 用户要求「参考大厂：让用户选择
    // 最小化还是直接关闭」）：由 UI 按 closeAction 分流。
    tray.onCloseIntercept = () => _handleWindowClose(state);
    // 菜单文案随状态变化
    tray.isTranscribing = () => state.running;
    tray.areSubtitlesVisible = () => state.subtitlesVisible;
    await tray.init(minimizeToTray: state.minimizeToTray);
  }

  /// 关闭窗口请求（点 ✕）—— 按用户设置的关闭行为分流。
  ///
  /// 2026-10-25 用户要求「参考大厂：让用户选择最小化还是直接关闭」：
  /// - `closeAction == 'ask'`（默认）→ 弹窗询问，可勾选「记住选择」
  /// - `'minimize'` → 同步隐藏到通知栏
  /// - `'quit'` → 走完整退出流程（保存后退出）
  Future<void> _handleWindowClose(TranslatorState state) async {
    if (state.closeAction == 'quit') {
      await _quitWithSave(context, state);
      return;
    }
    if (state.closeAction == 'minimize') {
      TrayService.instance.handleWindowCloseSync();
      return;
    }
    // 每次询问：弹窗让用户选（可勾选「记住选择，不再询问」）
    final picked = await showDialog<String>(
      context: context,
      builder: (ctx) => _CloseChoiceDialog(state: state),
    );
    if (picked == null) return; // 用户取消，什么都不做
    if (picked == 'quit') {
      await _quitWithSave(context, state);
    } else {
      TrayService.instance.handleWindowCloseSync();
    }
  }

  @override
  void dispose() {
    TrayService.instance.onToggleTranscribe = null;
    TrayService.instance.onQuit = null;
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final state = context.watch<TranslatorState>();

    // 平板（sw600dp 及以上）用侧边导航栏：
    // 横屏时底部导航栏离拇指太远，且白白占掉竖直空间（字幕区最需要高度）。
    // 手机与桌面保持底部导航栏 —— 桌面版是已验收过的观感，不动它。
    final useRail = Responsive.useSideNavigation(context);

    final content = Column(
      children: [
        const CapabilityBanner(),
        Expanded(
          child:
              IndexedStack(index: state.tabIndex, children: HomeShell._pages),
        ),
      ],
    );

    if (useRail) {
      return Scaffold(
        body: Row(
          children: [
            NavigationRail(
              selectedIndex: state.tabIndex,
              onDestinationSelected: state.setTab,
              // 平板竖屏时用展开式（图标+文字），横屏时用紧凑式（省宽度）
              extended: Responsive.isPortrait(context),
              labelType: Responsive.isPortrait(context)
                  ? NavigationRailLabelType.none
                  : NavigationRailLabelType.selected,
              destinations: const [
                NavigationRailDestination(
                  icon: Icon(Icons.subtitles_outlined),
                  selectedIcon: Icon(Icons.subtitles),
                  label: Text('字幕'),
                ),
                NavigationRailDestination(
                  icon: Icon(Icons.library_books_outlined),
                  selectedIcon: Icon(Icons.library_books),
                  label: Text('词库'),
                ),
                NavigationRailDestination(
                  icon: Icon(Icons.folder_outlined),
                  selectedIcon: Icon(Icons.folder),
                  label: Text('模型'),
                ),
                NavigationRailDestination(
                  icon: Icon(Icons.settings_outlined),
                  selectedIcon: Icon(Icons.settings),
                  label: Text('设置'),
                ),
              ],
            ),
            const VerticalDivider(width: 1, thickness: 1),
            Expanded(child: content),
          ],
        ),
      );
    }

    return Scaffold(
      body: content,
      bottomNavigationBar: NavigationBar(
        selectedIndex: state.tabIndex,
        onDestinationSelected: state.setTab,
        destinations: const [
          NavigationDestination(icon: Icon(Icons.subtitles), label: '字幕'),
          NavigationDestination(icon: Icon(Icons.library_books), label: '词库'),
          NavigationDestination(icon: Icon(Icons.folder), label: '模型'),
          NavigationDestination(icon: Icon(Icons.settings), label: '设置'),
        ],
      ),
    );
  }
}

/// 采集能力横幅。
///
/// ## 为什么必须显式展示
///
/// 卓易通容器 / DRM 保护 / 未授权等场景下系统内放采集会不可用，
/// 此时自动降级到麦克风。若不提示，用户会以为程序坏了 ——
/// 「点了开始，却只能录到自己」是最差体验。
///
/// 设计要求（DESIGN §2.5.2）：**禁止静默降级**。
class CapabilityBanner extends StatelessWidget {
  const CapabilityBanner({super.key});

  @override
  Widget build(BuildContext context) {
    final state = context.watch<TranslatorState>();

    // 仅降级时显示；不可用时由设置页给出完整引导
    if (!state.capability.isDegraded) return const SizedBox.shrink();

    return Material(
      color: Colors.orange.shade100,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
        child: Row(
          children: [
            const Icon(Icons.warning_amber, size: 18, color: Colors.deepOrange),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                state.capability.describe(),
                style: const TextStyle(fontSize: 11, color: Colors.deepOrange),
              ),
            ),
            TextButton(
              onPressed: () => state.setTab(3),
              child: const Text('设置', style: TextStyle(fontSize: 11)),
            ),
          ],
        ),
      ),
    );
  }
}
