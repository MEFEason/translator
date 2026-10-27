// 独立歌词字幕窗（QQ 音乐桌面歌词风格）。
//
// ## 方案：第二实例，而非多窗口
//
// 用户要求：主窗口保留转写+翻译，**另开一个独立无边框窗口**显示字幕
// （QQ 音乐桌面歌词那种），两个窗口并存。
//
// 为什么不加 desktop_multi_window / multi_window：
//   - multi_window 0.2.0 不支持 Windows（排除）
//   - desktop_multi_window 文档强烈建议配合 window_manager 的 **fork 版本**，
//     而本项目全部窗口/托盘逻辑基于标准 window_manager 0.5.2 写
//     （历史坑记录了一堆 0.5.2 专属问题）——换 fork 有动摇现有稳定性的风险
//   - 多窗口插件带原生 DLL，漏拷贝进产物会启动即崩（项目历史教训）
//
// 第二实例方案的优势：
//   - **零依赖、零 C++ 改动、零 pubspec 改动** —— 不动任何已验证稳定的部分
//   - 歌词窗 = 同一个 exe 带 `--lyrics` 参数启动的第二个进程
//   - 数据通道 = 主实例把最新字幕行写进共享 JSON，歌词实例轮询渲染
//   - 崩溃隔离：歌词窗崩了不影响主转写，反之亦然
//
// ## 数据通道（共享文件）
//
// 主实例（`TranslatorState._poll` 里字幕变化时）写：
//   %TEMP%\translator_lyrics.json
//     { "alive": true, "fontSize": 22, "lines": [{"s":"EN","t":"中文","sp":"对方"}] }
//
// 歌词实例（[LyricsApp] 每 100ms）读该文件渲染。
// 主实例退出（含崩溃）时文件停更 —— 歌词实例用**文件修改时间**判定存活
// （超过 5 秒未更新 → 显示「等待主程序…」并隐藏），比显式 alive 标志更健壮。

import 'dart:async';
import 'dart:convert';
import 'dart:ffi' hide Size;
import 'dart:io' show Directory, File, Platform, exit;

import 'package:ffi/ffi.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:path/path.dart' as p;
import 'package:win32/win32.dart' as win;
import 'package:window_manager/window_manager.dart';

/// 共享字幕文件路径（主实例写、歌词实例读）。
String lyricsSharedPath() {
  return p.join(Directory.systemTemp.path, 'translator_lyrics.json');
}

/// 字幕窗几何（尺寸/位置）持久化文件。
///
/// 放 %APPDATA%（不是 %TEMP% —— 临时目录会被系统清理，
/// 用户拖好的大小会丢）。
String _geomPath() {
  final base = Platform.environment['APPDATA'] ?? Directory.systemTemp.path;
  return p.join(base, 'Translator', 'lyrics_geom.json');
}

/// 读取上次保存的窗口矩形 [left, top, right, bottom]（物理像素）。
List<int>? readSavedRect() {
  try {
    final f = File(_geomPath());
    if (!f.existsSync()) return null;
    final j = jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
    final r = (j['rect'] as List?)?.map((e) => (e as num).round()).toList();
    if (r == null || r.length != 4) return null;
    // 基本合理性检查：宽高为正且不超屏（防止多屏变化后窗口跑到天外）
    if (r[2] - r[0] < 120 || r[3] - r[1] < 60) return null;
    return r;
  } catch (_) {
    return null;
  }
}

/// 主实例调用：把最新字幕行写进共享文件。
///
/// 只写**必要字段**（原文 / 译文 / 说话人 / 字号），歌词实例只依赖这些。
/// 写失败只记日志 —— 歌词窗是增强功能，不该拖累主转写。
void writeLyricsShared({
  required double fontSize,
  required List<Map<String, dynamic>> lines,
  String partial = '',
  String partialZh = '',
}) {
  try {
    final payload = {
      'ts': DateTime.now().millisecondsSinceEpoch,
      'fontSize': fontSize,
      'lines': lines, // 已锁定行（旧 → 新）
      'partial': partial, // 流式进行中的文本（字幕窗实时刷新）
      'partialZh': partialZh, // 流式译文（英→中实时）
      // 已回填译文的行数：任意一行译文到位都会让它变化。字幕窗
      // 靠它判定重绘 —— 比逐行比较更省，且能覆盖非最后一行的回填。
      'filled': lines.where((l) => l['t'] != null).length,
    };
    File(lyricsSharedPath()).writeAsStringSync(jsonEncode(payload));
  } catch (e) {
    debugPrint('[lyrics] 写共享文件失败（忽略）: $e');
  }
}

/// 歌词实例读取的快照。
class _LyricsSnapshot {
  final double fontSize;
  final List<({String s, String? t, String? sp})> lines; // 旧 → 新
  final DateTime updatedAt;

  /// 流式进行中的文本（主窗口的 partial）。字幕窗把它作为当前行
  /// 实时刷新 —— 用户方案：悬浮字幕一句都不切，说完才变锁定行。
  final String partial;

  /// 流式译文（英文预览对应中文，双语同步生长）。
  final String partialZh;

  /// 已回填译文的行数（任意行译文变化都会让它 +1，驱动重绘）
  final int filled;

  const _LyricsSnapshot(this.fontSize, this.lines, this.updatedAt, this.partial,
      this.partialZh, this.filled);
}

/// 歌词实例读取共享文件。
///
/// 返回 null 表示「主实例暂不可用」—— 依据是文件超过 [staleSec] 秒未更新。
/// ⚠️ 2026-10-25：阈值从 5 秒放宽到 20 秒 —— 主实例在「停止→重建会话」
/// （切声源后重开）时会同步初始化 WASAPI 采集设备，UI isolate 冻结
/// 可达数秒，心跳停写；5 秒阈值会把这种正常停顿误判成主程序已死。
_LyricsSnapshot? readLyricsShared({int staleSec = 20}) {
  final path = lyricsSharedPath();
  final f = File(path);
  if (!f.existsSync()) return null;
  try {
    final age = DateTime.now().difference(f.lastModifiedSync());
    if (age > Duration(seconds: staleSec)) return null;
    final j = jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
    final fs = (j['fontSize'] as num?)?.toDouble() ?? 22.0;
    final raw = (j['lines'] as List?) ?? const [];
    final lines = raw
        .whereType<Map<String, dynamic>>()
        .map((l) => (
              s: (l['s'] as String?) ?? '',
              t: l['t'] as String?,
              sp: l['sp'] as String?,
            ))
        .toList();
    final partial = (j['partial'] as String?) ?? '';
    final partialZh = (j['partialZh'] as String?) ?? '';
    final filled = (j['filled'] as num?)?.toInt() ?? 0;
    return _LyricsSnapshot(
        fs, lines, f.lastModifiedSync(), partial, partialZh, filled);
  } catch (_) {
    return null; // 文件正在写 / 格式坏 → 当作暂无数据
  }
}

/// 当前进程是否以歌词窗模式启动。
///
/// 用**环境变量** `TRANSLATOR_LYRICS=1` 判定，而不是命令行参数：
/// Windows 下 `Platform.executableArguments` 不可靠，而 `win32` 包的
/// `GetCommandLineW` 符号在部分链接场景下解析不到（实测构建报错）。
/// 环境变量由主实例 `Process.start` 时通过 `environment` 注入，
/// 跨进程传递最稳、也不引入任何额外依赖。
bool isLyricsMode() {
  return Platform.environment['TRANSLATOR_LYRICS'] == '1';
}

/// 歌词窗**基准**尺寸（scale=1.0 时；用户反馈 640x260 太大，收窄到 480x160）。
///
/// 实际大小 = 基准 × scale。scale 由设置页「歌词窗大小」滑块控制，
/// 经共享文件传给歌词实例实时应用。
const _lyricsSize = Size(480, 160);

/// 字幕窗**最小**尺寸（逻辑像素）。
///
/// 用户反馈：窗口可被拖成一条细线，界面全毁。WS_THICKFRAME 让
/// 用户自由拖边角调整大小，但要有下限兜底 —— 轮询检测到越界即拉回。
const _lyricsMinSize = Size(260, 90);

/// WM_NCLBUTTONDOWN：非客户区鼠标按下（拖动无边框窗的标准手法）。
const int _wmNcLButtonDown = 0x00A1;

/// HTCAPTION：命中「标题栏」区域。
const int _htCaption = 2;

/// 歌词窗进入「拖动」模态。
///
/// 与主窗口悬浮拖动不同：系统里此刻有**两个** FLUTTER_RUNNER 窗口
/// （主 + 歌词），用 EnumWindows 按类名找会歧义。
/// 但拖动发生时用户刚在歌词窗上按下 —— 它一定是**当前前台窗口**，
/// 用 `GetForegroundWindow()` 拿到的一定是歌词窗自己。
void startDragLyricsWindow() {
  // ⚠️ 必须用**本进程**的窗口句柄，不能用 GetForegroundWindow：
  // 系统里同时有主窗口 + 字幕窗（都是 FLUTTER_RUNNER 窗口），
  // 前台窗口未必是字幕窗自己 —— 实测会把别的窗口拖走、
  // 字幕窗反而纹丝不动。
  final h = _ownHwnd();
  if (h == 0) return;
  // ⚠️ 关键（2026-10-24 用户实测「拖不动 / 有缩放光标但拉不动」）：
  // 必须先 ReleaseCapture()！Flutter 引擎在鼠标按下时 SetCapture 了鼠标，
  // 残留捕获与 WM_NCLBUTTONDOWN 触发的系统拖拽/缩放模态循环冲突 →
  // 模态循环收不到后续鼠标消息，窗口纹丝不动。这是无边框 Flutter 窗
  // 做拖拽的标准配套（window_manager/titled_window 都这么做）。
  win.ReleaseCapture();
  // lParam 必须带**当前光标屏幕坐标**（loword=x, hiword=y）。
  // 传 0 时部分 Windows 版本按 (0,0) 算拖动偏移 → 字幕窗拖不动
  // （用户实测）。GetMessagePos 在 SendMessage 下未必刷新，
  // 直接 GetCursorPos 最稳。
  final pt = calloc<win.POINT>();
  win.GetCursorPos(pt);
  final x = pt.ref.x & 0xFFFF;
  final y = pt.ref.y & 0xFFFF;
  calloc.free(pt);
  final lparam = (y << 16) | x;
  win.SendMessage(h, _wmNcLButtonDown, _htCaption, lparam);
}

/// 底部手柄：进入「调整大小（向下）」模态。
///
/// HTBOTTOM=15：WM_NCLBUTTONDOWN 带此命中码 → 系统开始 SIZING，
/// 向下拖变高、向上变矮。顶边被固定（顶部现在负责拖动窗口）。
/// 最小尺寸仍由 _trackGeometry 兜底拉回。
///
/// 与 startDragLyricsWindow 同理：lParam 必须带真实光标坐标，传 0 会按
/// (0,0) 算偏移（实测顶边拖不动就是这么来的）。
void startResizeLyricsBottom() {
  final h = _ownHwnd();
  if (h == 0) return;
  win.ReleaseCapture(); // 同 startDragLyricsWindow：先释放 Flutter 的鼠标捕获
  const int htBottom = 15;
  final pt = calloc<win.POINT>();
  win.GetCursorPos(pt);
  final x = pt.ref.x & 0xFFFF;
  final y = pt.ref.y & 0xFFFF;
  calloc.free(pt);
  final lparam = (y << 16) | x;
  win.SendMessage(h, _wmNcLButtonDown, htBottom, lparam);
}

/// 歌词窗第二实例的入口。
///
/// ⚠️ **不走主窗口的 windowManager 初始化** —— 那是为主窗口设计的，
/// 且实测 `setAsFrameless()` 在第二实例上没生效（标题栏仍在）。
/// 这里直接用 win32 改窗口样式：
///   - `WS_POPUP` 替换 `WS_CAPTION|WS_THICKFRAME|WS_SYSMENU` → **无边框**
///   - `WS_EX_TOOLWINDOW` → **不进任务栏**、Alt+Tab 也不出现
///   - `WS_EX_TOPMOST` → **置顶**
/// 这是 Win32 桌面歌词窗的标准做法（QQ 音乐同款），不依赖任何插件行为。
Future<void> runLyricsWindow() async {
  WidgetsFlutterBinding.ensureInitialized();
  // 先渲染 UI（MaterialApp 提供 Directionality —— 裸 ListView 没有它
  // 会在 Scrollbar 构建时 Null check 崩溃，实测踩过），
  // 窗口样式在首帧后立刻改（runLyricsWindow 返回后的 microtask）。
  runApp(const LyricsApp());
  scheduleMicrotask(() {
    _applyLyricsWindowStyle();
    // 自检模式下留 proof：主实例靠它确认第二实例真的建了窗口
    // （runLyricsWindow 跑到这里 = 窗口初始化全流程无异常）
    if (Platform.environment['TRANSLATOR_SELFTEST'] == '1') {
      try {
        File(lyricsSharedPath().replaceAll(
                'translator_lyrics.json', 'translator_lyrics_proof.txt'))
            .writeAsStringSync("ok ${DateTime.now().toIso8601String()}");
      } catch (_) {}
    }
  });
}

/// 用 win32 直接给歌词窗改样式 + 定位。
void _applyLyricsWindowStyle() {
  final hwnd = _ownHwnd();
  if (hwnd == 0) {
    debugPrint('[lyrics] 找不到自己的窗口句柄，样式未改');
    return;
  }
  // 样式常量（WinUser.h 标准值，win32 包未导出全部，手写并注明来源）
  const int gwlStyle = -16;
  const int gwlExStyle = -20;
  const int wsCaption = 0x00C00000; // WS_CAPTION
  const int wsThickframe = 0x00040000;
  const int wsSysmenu = 0x00080000;
  const int wsMinimizeBox = 0x00020000;
  const int wsMaximizeBox = 0x00010000;
  const int wsBorder = 0x00800000;
  const int wsDlgframe = 0x00400000;
  const int wsPopup = 0x80000000;
  const int wsVisible = 0x10000000;
  const int wsExWindowEdge = 0x00000100;
  const int wsExToolwindow = 0x00000080;
  const int wsExTopmost = 0x00000008;
  const int swpNoZorder = 0x0004;
  const int swpNoActivate = 0x0010;
  const int swpFrameChanged = 0x0020;

  // 1) 无边框外观：WS_POPUP 替换标题栏/系统菜单，**保留 WS_THICKFRAME**。
  //
  // ⚠️ 保留 WS_THICKFRAME（2026-10-24 修正）：0.9.21 为治「顶边拖不动」
  //    把它整个删了，结果连底部调大小(HTBOTTOM)和拖动(HTCAPTION)一起失效 ——
  //    无边框后这俩命中码反而不灵。顶边拖不动的**真正原因**是 lParam=0，
  //    已在 0.9.20 用 GetCursorPos 修复。删边框是误伤，现恢复：
  //    8 方向可 resize，拖动走三区布局的 HTCAPTION，两者兼得。
  final style = win.GetWindowLongPtr(hwnd, gwlStyle);
  final newStyle = (style &
          ~(wsCaption |
              wsSysmenu |
              wsMinimizeBox |
              wsMaximizeBox |
              wsBorder |
              wsDlgframe)) |
      wsPopup |
      wsThickframe |
      wsVisible;
  win.SetWindowLongPtr(hwnd, gwlStyle, newStyle);

  // 2) 扩展样式：工具窗（不进任务栏）+ 置顶 + 去边缘
  //    + **WS_EX_LAYERED（2026-10-25 修「字幕窗全黑不透明」）**：
  //    Flutter 的每像素 alpha 在 Windows 上要求顶层窗口是分层窗口，
  //    否则合成器把半透明色直接铺在不透明底上 → 用户看到的就是一块死黑。
  const int wsExLayered = 0x00080000;
  final ex = win.GetWindowLongPtr(hwnd, gwlExStyle);
  final newEx =
      (ex & ~wsExWindowEdge) | wsExToolwindow | wsExTopmost | wsExLayered;
  win.SetWindowLongPtr(hwnd, gwlExStyle, newEx);

  // 3) 尺寸与位置：优先恢复用户上次拖好的大小/位置（物理像素），
  //    没有记录才用默认（右下角，基准尺寸 × DPI）。
  int x, y, w, h;
  final saved = readSavedRect();
  if (saved != null) {
    x = saved[0];
    y = saved[1];
    w = saved[2] - saved[0];
    h = saved[3] - saved[1];
  } else {
    final dpi = win.GetDpiForWindow(hwnd);
    final scale = dpi > 0 ? dpi / 96.0 : 1.0;
    w = (_lyricsSize.width * scale).round();
    h = (_lyricsSize.height * scale).round();
    final sw = win.GetSystemMetrics(0); // SM_CXSCREEN
    final sh = win.GetSystemMetrics(1); // SM_CYSCREEN
    x = sw - w - (48 * scale).round();
    y = sh - h - (160 * scale).round();
  }
  // ⚠️ 置顶必须把 hwndInsertAfter 传 HWND_TOPMOST(-1) ——
  // 只改 WS_EX_TOPMOST 样式位不生效（实测），这是 Win32 的坑。
  // 因此这里也不能再传 SWP_NOZORDER（会抵消 z-order 变更）。
  const int hwndTopmost = -1;
  win.SetWindowPos(
      hwnd, hwndTopmost, x, y, w, h, swpNoActivate | swpFrameChanged);

  // 4) 整窗半透明（2026-10-25 修「字幕窗全黑、看不到窗口后面内容」）
  //
  // Flutter Windows 不支持逐像素 alpha（长期已知限制），Dart 里
  // `Colors.black.withValues(alpha: 0.62)` 会被合成成**死黑不透明** ——
  // 用户看到的就是"一块全黑的板"。
  //
  // 解法：WS_EX_LAYERED + LWA_ALPHA —— 整个窗口（黑底和文字一起）
  // 以 bAlpha/255 的不透明度与桌面混合，正是"半透明字幕面板"的
  // 标准做法。200/255 ≈ 78%：黑底透出桌面，白字仍清晰。
  //
  // ⚠️ 加了 WS_EX_LAYERED 必须配这一句：分层窗口不调属性 = 整窗全透明
  // 直接消失（实测坑）。
  const int lwaAlpha = 0x00000002;
  win.SetLayeredWindowAttributes(hwnd, 0, 200, lwaAlpha);
  debugPrint('[lyrics] 字幕窗样式已应用（无边框/可拖/置顶/不进任务栏/整窗半透明）');
}

/// 本进程的歌词窗句柄。
///
/// EnumWindows 回调**必须是顶层静态函数**（Pointer.fromFunction 不接受闭包，
/// 与 tray_service 的 _enumWindowsProc 同一约束）。
int _lyricsHwndFound = 0;

int _enumOwnWindow(int hwnd, int lParam) {
  final pid = calloc<Uint32>();
  win.GetWindowThreadProcessId(hwnd, pid);
  final mine = pid.value == win.GetCurrentProcessId();
  calloc.free(pid);
  if (mine) {
    final cls = calloc<Uint16>(256).cast<Utf16>();
    win.GetClassName(hwnd, cls, 256);
    final name = cls.toDartString();
    calloc.free(cls.cast<Uint16>());
    if (name.contains('FLUTTER_RUNNER')) {
      _lyricsHwndFound = hwnd;
      return 0; // 找到即停
    }
  }
  return 1;
}

final _enumOwnWindowPtr =
    Pointer.fromFunction<win.WNDENUMPROC>(_enumOwnWindow, 0);

int _ownHwndCached = 0;

/// 找本进程的 Flutter 主窗口句柄（字幕实例只有一个窗口，无歧义）。
///
/// 句柄在进程生命周期内不变 —— 缓存它，避免 _trackGeometry
/// 每 100ms 全量 EnumWindows。
int _ownHwnd() {
  if (_ownHwndCached != 0) return _ownHwndCached;
  _lyricsHwndFound = 0;
  win.EnumWindows(_enumOwnWindowPtr, 0);
  _ownHwndCached = _lyricsHwndFound;
  return _ownHwndCached;
}

/// 歌词窗根组件：轮询共享文件，渲染 QQ 音乐桌面歌词风格。
class LyricsApp extends StatefulWidget {
  const LyricsApp({super.key});

  @override
  State<LyricsApp> createState() => _LyricsAppState();
}

class _LyricsAppState extends State<LyricsApp> {
  Timer? _timer;
  _LyricsSnapshot? _snap;

  /// 是否曾经显示过字幕内容 —— 区分两种占位文案：
  /// 从没来过 =「等待主程序开始转写」；来过又停更 =「主程序未发送字幕，
  /// 请稍候」（切声源重建会话、主实例冻结数秒属此类，2026-10-25 用户方案：
  /// **不判死，显示等待提示**）。
  bool _everHadContent = false;

  /// 连续「主程序不可用」的轮询次数（60ms 一次，>5000 ≈ 5 分钟才自杀）。
  ///
  /// ⚠️ 2026-10-25：旧阈值 600（≈36 秒）会在「切声源重建会话」时误杀 ——
  /// 用户方案改为超时显示等待提示、不判死；自杀仅保留为**孤儿窗口兜底**
  /// （主程序被强杀、没人 kill 本进程的场景），5 分钟足够覆盖任何正常停顿。
  int _stalePolls = 0;

  @override
  void initState() {
    super.initState();
    // 启动时先同步读一次 —— 不等 100ms 定时器，
    // 避免第一帧在「快照还没读到」的状态下构建（之前崩溃的根因之一）。
    _snap = readLyricsShared();
    // 60ms：流式双语上屏延迟的主控旋钮之一（旧 100ms）。纯读一个
    // 几 KB 的 JSON，开销可忽略；越低越跟手，但再低收益递减。
    _timer = Timer.periodic(const Duration(milliseconds: 60), (_) {
      final s = readLyricsShared();
      // 主程序长时间不可用 → 字幕窗自行退出（自愈）。
      // 覆盖场景：主程序被强杀/崩溃，dispose 没跑、没人 kill 本进程，
      // 孤儿窗口会一直挂着 —— 用户看到「多个窗口并行」就是这么来的。
      // 不去按 PID 杀别人：Windows 会复用 PID，误杀风险大；
      // 自己退出最安全。
      if (s == null) {
        _stalePolls++;
        if (_stalePolls > 5000) {
          debugPrint('[lyrics] 主程序超过 5 分钟不可用，字幕窗自行退出（孤儿兜底）');
          exit(0);
        }
      } else {
        _stalePolls = 0;
        _everHadContent = true;
      }
      // 内容变化才 setState（100ms 一次，无变化时别白白重建子树）
      final changed = (s == null) != (_snap == null) ||
          (s != null &&
              _snap != null &&
              (s.fontSize != _snap!.fontSize ||
                  s.lines.length != _snap!.lines.length ||
                  s.partial != _snap!.partial || // ★ 必须比较 partial，否则流式永不重绘
                  s.partialZh != _snap!.partialZh ||
                  // ★ 用 filled 计数捕获译文回填（2026-10-24「主窗口出中文、
                  // 字幕窗还是英文」根因：译文到位时英文不变，逐行比较 .s 漏判。
                  // filled 覆盖任意行（含非最后一行）的回填，比逐行比较更稳）
                  s.filled != _snap!.filled ||
                  (s.lines.isNotEmpty &&
                      _snap!.lines.isNotEmpty &&
                      s.lines.last.s != _snap!.lines.last.s)));
      if (changed) setState(() => _snap = s);
      // 用户可能刚拖过边角改了窗口大小 —— 检测矩形变化并持久化
      _trackGeometry();
    });
  }

  // ── 尺寸/位置持久化 ──
  /// 上一次记录的窗口矩形（物理像素）。与当前值不同 = 用户拖动过。
  List<int>? _lastRect;

  /// 防抖：拖动过程中连续变化，停手 600ms 后才落盘。
  Timer? _geomDebounce;

  /// 检测窗口矩形变化（用户拖边角/移动），稳定后写入几何文件。
  void _trackGeometry() {
    final hwnd = _ownHwnd();
    if (hwnd == 0) return;
    final r = calloc<win.RECT>();
    win.GetWindowRect(hwnd, r);
    final cur = [r.ref.left, r.ref.top, r.ref.right, r.ref.bottom];
    calloc.free(r);
    if (_lastRect == null) {
      _lastRect = cur;
      return;
    }
    // 最小尺寸兜底：被拖成细线时立刻拉回（用户反馈的问题 1）
    final dpi = win.GetDpiForWindow(hwnd);
    final sc = dpi > 0 ? dpi / 96.0 : 1.0;
    final minW = (_lyricsMinSize.width * sc).round();
    final minH = (_lyricsMinSize.height * sc).round();
    final w = cur[2] - cur[0];
    final h = cur[3] - cur[1];
    if (w < minW || h < minH) {
      const int hwndTopmost = -1;
      const int swpNoActivate = 0x0010;
      win.SetWindowPos(hwnd, hwndTopmost, cur[0], cur[1], w < minW ? minW : w,
          h < minH ? minH : h, swpNoActivate);
      _lastRect = null; // 拉回后下一轮重新读取
      return;
    }
    if (listEquals(_lastRect!, cur)) return;
    _lastRect = cur;
    _geomDebounce?.cancel();
    _geomDebounce = Timer(const Duration(milliseconds: 600), () {
      try {
        final f = File(_geomPath());
        f.parent.createSync(recursive: true);
        f.writeAsStringSync(jsonEncode({'rect': _lastRect}));
        debugPrint('[lyrics] 窗口尺寸/位置已保存: $_lastRect');
      } catch (e) {
        debugPrint('[lyrics] 保存几何失败（忽略）: $e');
      }
    });
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    // MaterialApp 不能省：ListView 的 Scrollbar 需要 Directionality，
    // 没有它第一帧就 Null check 崩溃（实测）。透明背景由下面的
    // Container 自己画，Material 层保持透明。
    return MaterialApp(
      debugShowCheckedModeBanner: false,
      home: Scaffold(
        backgroundColor: Colors.transparent,
        body: _buildBody(),
      ),
    );
  }

  Widget _buildBody() {
    // 深色半透明面板铺满窗口（不铺满会露出 Flutter 默认灰底）。
    //
    // 三区结构（2026-10-24 用户定案：顶部固定用于拖动，只有底部可调大小）：
    //   顶部条(20px)  = 拖动窗口（HTCAPTION）—— 不会被 resize 抢
    //   中部列表       = 字幕内容（滚轮可滚；按下同样拖动窗口）
    //   底部条(12px)   = 调大小（HTBOTTOM）：向下拖变高、向上变矮
    // 系统级 WS_THICKFRAME 整圈 resize 已移除，见 _applyLyricsWindowStyle。
    return Container(
      width: double.infinity,
      height: double.infinity,
      color: Colors.black.withValues(alpha: 0.62),
      child: Column(
        children: [
          // ── 顶部：拖动条 ──
          GestureDetector(
            onPanDown: (_) => startDragLyricsWindow(),
            behavior: HitTestBehavior.opaque,
            child: Container(
              height: 20,
              alignment: Alignment.center,
              child: Container(
                width: 36,
                height: 4,
                decoration: BoxDecoration(
                  color: Colors.white30,
                  borderRadius: BorderRadius.circular(2),
                ),
              ),
            ),
          ),
          // ── 中部：字幕列表（按下也拖动窗口）──
          Expanded(
            child: GestureDetector(
              onPanDown: (_) => startDragLyricsWindow(),
              behavior: HitTestBehavior.translucent,
              child: _snap != null ? _buildLyrics() : _buildWaiting(),
            ),
          ),
          // ── 底部：调大小条 ──
          GestureDetector(
            onPanDown: (_) => startResizeLyricsBottom(),
            behavior: HitTestBehavior.opaque,
            child: MouseRegion(
              cursor: SystemMouseCursors.resizeRow,
              child: Container(
                height: 12,
                alignment: Alignment.center,
                child: Container(
                  width: 60,
                  height: 3,
                  decoration: BoxDecoration(
                    color: Colors.white24,
                    borderRadius: BorderRadius.circular(2),
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }

  /// 主实例暂不可用时的占位（半透明提示，不刺眼）。
  ///
  /// 两种文案（2026-10-25 用户方案：重建会话期间**不判死、显示等待**）：
  /// - 从没收到过内容 → 等待开始
  /// - 收到过又停更 → 主程序未发送（切声源重建会话就属于这种，恢复后自动续上）
  Widget _buildWaiting() {
    return Center(
      child: Text(
        _everHadContent ? '主程序未发送字幕，请稍候…' : '等待主程序转写…',
        style: const TextStyle(fontSize: 15, color: Colors.white38),
      ),
    );
  }

  /// 双语字幕面板（与主窗口悬浮字幕一致，用户指定的目标形态）：
  /// 倒序列表，最新在底部；每行英文白字在上、中文 amber 在下。
  Widget _buildLyrics() {
    final snap = _snap;
    if (snap == null) return _buildWaiting(); // 双保险：绝不空指针
    final base = snap.fontSize;
    // 流式形态（用户 2026-10-23 定案）：悬浮字幕**一句都不切** ——
    // 锁定行（final）按序在上，当前 partial 作为最新一行实时刷新，
    // 说完才变锁定行。ListView(reverse) 最新在底部，超限自动滚动，
    // 主实例只传最近 80 行锁定行（窗口上限），旧内容自然丢弃。
    return ListView(
      reverse: true,
      padding: EdgeInsets.zero,
      children: [
        // ★ 当前行：流式生长中（**始终存在**，是列表的一部分）。
        //   用户定案（2026-10-24）：不要在外面单独挂一行 partial，
        //   而是「最新一行本身在长」—— 先占位符，文字一个一个填进来，
        //   说完才定型成锁定行，然后新的占位符开始。
        Padding(
          padding: const EdgeInsets.only(bottom: 10),
          child: _streamingLine(snap.partial, snap.partialZh, base),
        ),
        // 锁定行（旧→新，reversed 后新→旧）。说话人去重（2026-10-24 用户：
        // 同一人讲话标签只占一行，别每行都顶个【说话人】浪费纵向空间）：
        // 只在「段落首行」（与更旧一行说话人不同）显示标签。
        for (var i = snap.lines.length - 1; i >= 0; i--)
          Padding(
            padding: const EdgeInsets.only(bottom: 10),
            child: _bilingualLine(
              snap.lines[i],
              base,
              // i==0 是最旧行=某段首行；否则看它前一行是否换了人
              showSpeaker: i == 0 || snap.lines[i - 1].sp != snap.lines[i].sp,
            ),
          ),
      ],
    );
  }

  /// 流式当前行：有文本就显示「文本 + 光标」，还没出字就只显示
  /// **等候占位符**（半透明 ▍），让「正在听」这件事可见。
  Widget _streamingLine(String text, String zh, double base) {
    final t = text.trim();
    final z = zh.trim();
    if (t.isEmpty && z.isEmpty) {
      // 占位符：等第一字出来（带轻微呼吸感，避免看着像卡住）
      return _BlinkingCursor(size: base * 1.05);
    }
    // 双语流式：英文在上（白）、中文译文在下（amber，与锁定行同色）
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (t.isNotEmpty) _streamingEn(t, base),
        if (z.isNotEmpty)
          Text(
            z,
            style: TextStyle(
              color: const Color(0xFFFFC107),
              fontSize: base * 0.9,
              height: 1.3,
              shadows: const [Shadow(blurRadius: 6, color: Colors.black54)],
            ),
          ),
      ],
    );
  }

  Widget _streamingEn(String t, double base) {
    return RichText(
      text: TextSpan(
        children: [
          TextSpan(
            text: t,
            style: TextStyle(
              color: Colors.white,
              fontSize: base * 1.05,
              height: 1.3,
              fontWeight: FontWeight.w500,
              shadows: const [Shadow(blurRadius: 6, color: Colors.black54)],
            ),
          ),
          WidgetSpan(
            alignment: PlaceholderAlignment.middle,
            child: _BlinkingCursor(size: base * 1.05),
          ),
        ],
      ),
    );
  }

  /// 闪烁光标（▍）：既是「正在识别」的光标，也是还没出字时的等候占位符。
  ///
  /// 为什么要有它：用户要求「带等候占位符，然后一个一个出来」——
  /// 一句刚开始时列表里必须有个东西表示「在听」，否则看着像没反应。
  Widget _BlinkingCursor({required double size}) => _BlinkCursor(size: size);

  /// 一行双语字幕：说话人小标签 + 英文（白）在上 + 中文（amber）在下。
  ///
  /// [showSpeaker] 为 false 时不渲染说话人行 —— 同一人连续讲话时，
  /// 只有段落首行显示【说话人】，后续行省出纵向空间（用户 2026-10-24 要求）。
  Widget _bilingualLine(({String s, String? t, String? sp}) line, double base,
      {bool showSpeaker = true}) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (showSpeaker && line.sp != null && line.sp!.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(bottom: 2),
            child: Text(
              '【${line.sp}】',
              style: TextStyle(
                fontSize: base * 0.6,
                color: Colors.white.withValues(alpha: 0.55),
              ),
            ),
          ),
        if (line.s.isNotEmpty)
          Text(
            line.s,
            style: TextStyle(
              color: Colors.white,
              fontSize: base,
              height: 1.3,
              shadows: const [Shadow(blurRadius: 6, color: Colors.black54)],
            ),
          ),
        if (line.t != null && line.t!.isNotEmpty)
          Text(
            line.t!,
            style: TextStyle(
              color: Colors.amber.shade200,
              fontSize: base * 0.92,
              height: 1.3,
              shadows: const [Shadow(blurRadius: 6, color: Colors.black54)],
            ),
          ),
      ],
    );
  }
}

/// 呼吸式闪烁的光标 ▍。
class _BlinkCursor extends StatefulWidget {
  final double size;
  const _BlinkCursor({required this.size});

  @override
  State<_BlinkCursor> createState() => _BlinkCursorState();
}

class _BlinkCursorState extends State<_BlinkCursor>
    with SingleTickerProviderStateMixin {
  late final AnimationController _c;

  @override
  void initState() {
    super.initState();
    _c = AnimationController(
      vsync: this,
      duration: const Duration(milliseconds: 900),
    )..repeat(reverse: true);
  }

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return FadeTransition(
      opacity: Tween<double>(begin: 0.25, end: 0.9).animate(_c),
      child: Text(
        '▍',
        style: TextStyle(
          color: Colors.white70,
          fontSize: widget.size,
          height: 1.3,
          shadows: const [Shadow(blurRadius: 6, color: Colors.black54)],
        ),
      ),
    );
  }
}
