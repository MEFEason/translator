import 'dart:async';
import 'dart:convert';
import 'dart:io'
    show
        Platform,
        Process,
        File,
        Directory,
        FileMode,
        pid,
        ProcessSignal;

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../ffi/bridge.dart';
import '../services/android_channel.dart';
import '../services/tray_service.dart';
import '../services/lyrics_window.dart';

/// 字幕行（与 Rust 侧 `SubtitleLine` 对应）。
class SubtitleLine {
  /// 引擎行序号（add 事件带）—— fill 回填靠它精确匹配。
  final int seq;
  final String source;
  final String? target;
  final int startMs;
  final int endMs;

  /// 哨兵往返校验失败 —— 术语可能被误译，需人工复核
  final bool needsReview;

  /// 因超长被强制切分
  final bool forcedCut;

  /// 说话人标签（多声源时区分「我」与「对方」）
  final String? speaker;

  const SubtitleLine({
    required this.seq,
    required this.source,
    this.target,
    required this.startMs,
    required this.endMs,
    this.needsReview = false,
    this.forcedCut = false,
    this.speaker,
  });

  bool get isTranslating => target == null && !needsReview;

  /// SRT 时间戳 `HH:MM:SS,mmm`
  String get srtStamp {
    final h = startMs ~/ 3600000;
    final m = (startMs % 3600000) ~/ 60000;
    final s = (startMs % 60000) ~/ 1000;
    final ms = startMs % 1000;
    return '${h.toString().padLeft(2, '0')}:'
        '${m.toString().padLeft(2, '0')}:'
        '${s.toString().padLeft(2, '0')},'
        '${ms.toString().padLeft(3, '0')}';
  }
}

/// 采集能力（与 Rust 侧 `CaptureCapability` 对应）。
///
/// 关键：`isDegraded` 为 true 时**必须在 UI 显式展示**，
/// 禁止静默降级（DESIGN §2.5.2）。
class CaptureCapability {
  final bool systemAudio;
  final bool available;
  final String reason;

  const CaptureCapability.system()
      : systemAudio = true,
        available = true,
        reason = '';

  const CaptureCapability.mic(String this.reason)
      : systemAudio = false,
        available = true;

  const CaptureCapability.unavailable(String this.reason)
      : systemAudio = false,
        available = false;

  bool get isDegraded => available && !systemAudio;

  String describe() {
    if (systemAudio) return '内放采集：可用';
    if (!available) return '音频采集不可用：$reason';
    return '内放采集：不可用（$reason）\n已切换为麦克风，外放声音需佩戴耳机。';
  }
}

/// 全局状态。
///
/// 单一 ChangeNotifier 承载全部 UI 状态 —— 本应用状态量不大
/// （字幕 + 能力 + 词库），拆多个 provider 反而增加同步成本。
class TranslatorState extends ChangeNotifier {
  TranslatorState({Bridge? bridge}) : _bridge = bridge ?? Bridge();

  final Bridge _bridge;

  /// 字幕轮询定时器。100ms 一次 —— 比字幕出现频率快得多，
  /// 又远低于 FFI 调用的开销（10~50μs），不会拖累 UI。
  Timer? _pollTimer;

  /// 内存窗口外的更早字幕（从转写盘分页加载），旧 → 新
  final List<SubtitleLine> _history = [];

  /// 是否还有更早的内容可从转写盘取
  bool _hasMoreHistory = true;

  /// 正在加载历史（防止重复触发）
  bool _loadingHistory = false;

  // ── 状态 ──
  int _tabIndex = 0;
  bool _running = false;
  List<SubtitleLine> _lines = [];
  String _partial = '';

  /// 流式译文（与 _partial 同步，英文预览的实时中文）。
  String _partialZh = '';

  /// 采集能力。
  ///
  /// 初值是 **unavailable('尚未探测')** 而不是乐观的 system() ——
  /// 后者会让 UI 先显示"系统内放采集可用"的绿勾，而实际上什么都没问过。
  /// 这是 DESIGN §2.5.2「禁止静默降级」的要求：没探测就不许说可用。
  CaptureCapability _capability = const CaptureCapability.unavailable('尚未探测');

  /// 会话状态快照
  SessionStatus _status = const SessionStatus(state: SessionState.created);

  /// 启动自检结果
  List<SelfcheckStep> _steps = const [];

  /// 转写盘文件路径（UI 展示给用户）
  ///
  /// ⚠️ 语义纠正（2026-10-25）：这里装的是**目录**，不是文件路径。
  /// 来源是 `_bridge.transcriptDir()`（Rust `default_transcript_dir()`，
  /// 即 `<桌面>/translate results`）。UI 曾经对它做
  /// `File(path).parent.path`，把目录当文件取父目录 → 显示成「桌面」，
  /// 用户看不到真正的 `translate results`。现在直接展示目录本身。
  String _transcriptPath = '';

  /// 持久化 key：用户自定义的转写盘位置
  static const _kTranscriptDirKey = 'transcript_dir';

  /// 转写盘统计
  TranscriptStats _transcriptStats = const TranscriptStats(0, 0, 0);

  /// 会话错误详情（模型缺失 / 词库语法错 / 采集不可用）
  String _sessionError = '';

  /// **探测到的模型目录**（bootstrap 时填充）。
  ///
  /// ## 为什么必须存在状态里
  ///
  /// 以前模型页直接调 `bridge.discoverModels()`（Rust 侧探测），
  /// 而 Rust 的 `discover_model_layout` 只探「exe 相邻目录」与
  /// 「cwd/models」—— **Android 上 current_exe 是 .so 所在目录**
  /// （app/lib/arm64），那三处全都不存在 → 必然返回空 →
  /// 模型页一片空白（2026-10-28 用户反馈「模型页面还是显示里面没东西」）。
  ///
  /// Android 的真实落点是应用私有目录
  /// `<internalFilesDir>/models/{asr,mt,offline,glossary}`，
  /// bootstrap 里已用 `_probeAndroidModels` 探到 —— 存在这里给 UI 复用。
  ModelPaths models = const ModelPaths('', '', '');

  /// 是否已 bootstrap 完成
  bool _bootstrapped = false;

  /// 是否处于暂停状态（暂停 ≠ 停止：暂停可恢复）
  bool _paused = false;

  /// 降级原因（能力降级时 UI 展示）
  String? _degradeReason;

  /// 运行时环境：'native' | 'zhuoyitong'
  String _runtime = 'native';

  /// 采集来源：'systemAudio' | 'microphone'（单一声源时的快捷选择）
  String _source = 'systemAudio';

  /// 已启用的声源。多选 —— 可同时转译内放与麦克风。
  ///
  /// 默认两个都开：双语对话场景下，既要看到对方说了什么，
  /// 也要看到自己说了什么（译文可回听）。
  final Set<String> _sources = {'systemAudio', 'microphone'};

  /// 说话人标签
  String _speakerLocal = '我';
  String _speakerRemote = '对方';

  /// 性能档位：'stable' | 'fast'
  String _profile = 'stable';

  /// 资源档位：'light' | 'balanced' | 'full'
  ///
  /// ⚠️ 2026-10-25：Rust 侧曾**从未解析**该字段（用户在设置页切档完全无效，
  /// 已修）。现在 UI 主入口是**线程数 Slider**（`_asrThreads`），
  /// 该字段退化为语义标签，仅用于日志与兼容路径。
  String _resourceTier = 'full';

  /// 识别（ASR）线程数，1-8。设置页 Slider 直接拖动选择。
  ///
  /// ⚠️ 2026-10-25 占用基准实测（bench_run.py，四组对照）后
  /// **默认从 8 降到 4**：
  /// - 质量与线程数**无关**（四组配置质量数据完全一致，模型精度
  ///   由模型决定，线程只影响速度余量）
  /// - 实时喂入下 2 线程都能跟上（RTF≈1.0）；4 线程 = 一倍余量，
  ///   覆盖快语速连讲的高峰
  /// - 每 +1 线程实测 ≈ +1.6 核真实功耗（ORT 自旋），8 线程
  ///   在 20 核机占 63% 系统 CPU / 2GB 内存，属于纯浪费且会
  ///   抢会议软件的时间片（= 用户说的"卡顿"）
  int _asrThreads = 4;

  /// 翻译（MT）线程数，1-6。随识别线程联动：`min(识别, 6)`
  /// （翻译是流水线后段，没必要比识别更多）。
  int _mtThreads = 4;

  /// VAD hangover（毫秒）—— 句尾静音多久后断句。
  /// 2026-10-25 起可在转写中实时调整（Slider）。
  int _hangoverMs = 600;

  /// 离线精修（两遍解码第二遍）开关。转写中可实时切换。
  bool _refineEnabled = true;

  /// 最近一次系统负载探测结果（null = 还没探过）
  SystemLoad? _systemLoad;

  /// 是否正在探测系统负载
  bool _probingLoad = false;

  /// 资源档位的静态说明
  List<ResourceTierInfo> _tierInfos = const [];

  /// 采集丢帧数（健康度）
  int _droppedFrames = 0;

  // ── 读取 ──
  int get tabIndex => _tabIndex;
  bool get running => _running;
  List<SubtitleLine> get lines => List.unmodifiable(_lines);
  String get partial => _partial;
  String get partialZh => _partialZh;
  CaptureCapability get capability => _capability;
  String? get degradeReason => _degradeReason;
  String get runtime => _runtime;
  String get profile => _profile;
  String get resourceTier => _resourceTier;
  int get asrThreads => _asrThreads;
  int get mtThreads => _mtThreads;

  /// 当前断句灵敏度（VAD hangover，毫秒）
  int get hangoverMs => _hangoverMs;

  /// 离线精修开关（两遍解码第二遍）
  bool get refineEnabled => _refineEnabled;

  /// 切换离线精修（转写中实时生效，下一句起用新值）。
  Future<void> setRefineEnabled(bool v) async {
    if (_refineEnabled == v) return;
    _refineEnabled = v;
    notifyListeners();
    await updateLiveRefine(v);
  }

  SystemLoad? get systemLoad => _systemLoad;
  bool get probingLoad => _probingLoad;
  List<ResourceTierInfo> get tierInfos => _tierInfos;
  String get source => _source;

  /// 探测系统资源占用，并按占用率给出档位建议。
  ///
  /// **只建议，不自动改** —— 用户已经选好的档位不该被系统偷偷改掉。
  Future<void> probeSystemLoad() async {
    if (_probingLoad) return;
    _probingLoad = true;
    notifyListeners();
    try {
      // 底层要 250ms 采样，丢到微任务外避免阻塞首帧
      final load = await Future(() => _bridge.probeSystemLoad());
      _systemLoad = load;
      if (_tierInfos.isEmpty) {
        _tierInfos = _bridge.resourceTiers();
      }
    } catch (_) {
      _systemLoad = null;
    } finally {
      _probingLoad = false;
      notifyListeners();
    }
  }

  /// 采用系统推荐的档位（用户在提示里点「采用建议」时调用）。
  ///
  /// 2026-10-25：优先采用**线程数推荐**（基于实测成本模型，比三档粒度更细）；
  /// 旧 DLL 没给该字段时退回档位键换算（1/2/8）。
  void adoptRecommendedTier() {
    final load = _systemLoad;
    if (load == null) return;
    final rec = running ? load.recThreadsRunning : load.recThreadsIdle;
    if (rec > 0) {
      updateLiveThreads(rec);
      return;
    }
    final key = load.recommended;
    int threads;
    if (key == 'light') {
      threads = 1;
    } else if (key == 'balanced') {
      threads = 2;
    } else {
      threads = 8;
    }
    setAsrThreads(threads);
  }

  /// 当前状态下的推荐线程数（0 = 探测结果里没有该字段）。
  int get recommendedThreads {
    final load = _systemLoad;
    if (load == null) return 0;
    return running ? load.recThreadsRunning : load.recThreadsIdle;
  }

  /// 设置识别线程数（Slider 拖动入口）。
  ///
  /// 翻译线程随之联动 `min(识别, 6)`；`resourceTier` 语义标签同步刷新，
  /// 保证日志与旧路径读到的仍是合法档位键。
  void setAsrThreads(int v) {
    final asr = v.clamp(1, 8);
    final mt = asr.clamp(1, 6);
    if (_asrThreads == asr && _mtThreads == mt) return;
    _asrThreads = asr;
    _mtThreads = mt;
    _resourceTier = asr <= 1
        ? 'light'
        : asr <= 2
            ? 'balanced'
            : 'full';
    notifyListeners();
  }

  /// **实时**调整识别线程数（2026-10-25：转写中拖动 Slider 即时生效）。
  ///
  /// 与 `setAsrThreads` 的区别：会通过 FFI 通知正在运行的引擎。
  /// - `hangover_ms` / `offline_refine` 下一句立即生效
  /// - 线程数在**下一句边界**热切换 recognizer（sherpa 线程数创建时定死），
  ///   切换后第一句段首质量可能短暂下降 —— 这是有文档的取舍。
  ///
  /// 未创建会话时退化为纯本地状态更新（下次建会话生效）。
  Future<void> updateLiveThreads(int threads) async {
    final t = threads.clamp(1, 8);
    setAsrThreads(t);
    final rc = _bridge.updateLiveConfig(
      jsonEncode({'asr_threads': _asrThreads}),
    );
    debugPrint('[live] 线程实时更新 → $_asrThreads（rc=$rc）');
  }

  /// **实时**开关离线精修（两遍解码第二遍），下一句立即生效。
  Future<void> updateLiveRefine(bool enabled) async {
    final rc = _bridge.updateLiveConfig(
      jsonEncode({'offline_refine': enabled}),
    );
    debugPrint('[live] 离线精修实时${enabled ? '开启' : '关闭'}（rc=$rc）');
  }

  /// **实时**调整断句灵敏度（VAD hangover，毫秒），下一句立即生效。
  ///
  /// 值越小越跟手（句间一停就断），越大越不容易碎句。
  Future<void> updateLiveHangover(int ms) async {
    final m = ms.clamp(200, 2000);
    _hangoverMs = m;
    notifyListeners();
    final rc = _bridge.updateLiveConfig(jsonEncode({'hangover_ms': m}));
    debugPrint('[live] hangover 实时更新 → $m ms（rc=$rc）');
  }

  void setResourceTier(String v) {
    if (_resourceTier == v) return;
    _resourceTier = v;
    notifyListeners();
  }

  void setSource(String v) {
    if (_source == v) return;
    _source = v;
    notifyListeners();
  }

  Set<String> get sources => Set.unmodifiable(_sources);
  String get speakerLocal => _speakerLocal;
  String get speakerRemote => _speakerRemote;

  void setSpeaker(String v) {
    if (_source == v) return;
    _source = v;
    notifyListeners();
  }

  void setSpeakerLabel(String kind, String label) {
    if (kind == 'microphone') {
      _speakerLocal = label;
    } else {
      _speakerRemote = label;
    }
    notifyListeners();
  }

  /// 切换某个声源的启用状态。至少保留一个 —— 全关等于没得转。
  ///
  /// ## 运行中也要能实时切换（2026-10-28 用户需求）
  ///
  /// 旧实现只改了 `_sources` 这个 **UI 状态集合**，而实际采集器
  /// 是**会话创建时**按 `cfg.sources` 定死的 → 用户在转写中点了开关，
  /// 界面变了但声音没变，必须先停止再开始，体验上就是「不能实时切换」。
  ///
  /// 现在：
  /// - **运行中** → 调平台接口真正启停那一路（Android 走 Kotlin 的
  ///   `AudioCaptureService`，桌面端走 Rust 的
  ///   `translator_session_set_source_enabled`）
  /// - **未运行** → 只改状态集合，下次 `start` 时按新集合起采集
  ///
  /// 停掉的那一路：Rust 侧的接收端一直等不到数据 → 该路不产生字幕，
  /// 另一路不受影响（两路采集器本来就是各自独立的）。
  void toggleSource(String kind, bool on) {
    if (on) {
      _sources.add(kind);
    } else {
      if (_sources.length <= 1) return; // 不允许全关
      _sources.remove(kind);
    }
    notifyListeners();
    if (_running) {
      unawaited(_applySourceToggle(kind, on));
    }
  }

  /// 把声源开关真正落到采集器上（运行中切换用）。
  ///
  /// 失败时**回滚 UI 状态** —— 否则界面显示与实际采集不一致，
  /// 用户会以为在录其实没录（比报错更糟）。
  Future<void> _applySourceToggle(String kind, bool on) async {
    const kindSystemAudio = 0;
    const kindMicrophone = 1;
    final k = kind == 'systemAudio' ? kindSystemAudio : kindMicrophone;
    var ok = false;
    if (AndroidChannel.instance.isAndroid) {
      ok = await AndroidChannel.instance.setSourceEnabled(k, on);
    } else {
      final rc = _bridge.sessionSetSourceEnabled(k, on);
      ok = rc == 0;
      if (!ok) debugPrint('[transcribe] 桌面端切换采集源失败（rc=$rc）');
    }
    debugPrint('[transcribe] 采集源实时切换：$kind → ${on ? "开" : "关"}（ok=$ok）');
    if (ok) return;

    // 回滚 UI 状态，保持与实际采集一致
    if (on) {
      _sources.remove(kind);
    } else {
      _sources.add(kind);
    }
    _sessionError = '切换采集源失败（$kind）—— 请检查权限或设备占用';
    notifyListeners();
  }

  int get droppedFrames => _droppedFrames;

  SessionStatus get status => _status;
  List<SelfcheckStep> get selfcheckSteps => _steps;
  String get transcriptPath => _transcriptPath;

  /// 翻译引擎是否可用。
  ///
  /// 阶段1（Android 只装 ASR 模型）时 Rust 侧降级为「仅识别」模式，
  /// `mtReady` 为 0 —— UI 据此明示「翻译引擎未启用」，
  /// **禁止静默**（DESIGN §2.5.2）。
  bool get mtAvailable => _status.mtReady;

  /// 当前转写盘位置是否为默认位置（桌面 `translate results`）。
  ///
  /// 用于 UI 标注「（默认位置）」/「（自定义）」。
  bool get isDefaultTranscriptDir {
    if (_transcriptPath.isEmpty) return true;
    return _transcriptPath == _bridge.transcriptDir();
  }

  /// 更改转写盘位置（持久化，重启后仍生效）。
  ///
  /// ⚠️ **只影响下一次转写**：当前会话的转写盘在 `sessionCreate` 时
  /// 就已确定。要立刻写到新位置，需结束本场转写再重新开始。
  Future<void> setTranscriptDir(String dir) async {
    if (dir.isEmpty || dir == _transcriptPath) return;
    _transcriptPath = dir;
    notifyListeners();
    final prefs = await SharedPreferences.getInstance();
    await prefs.setString(_kTranscriptDirKey, dir);
    debugPrint('[transcript] 位置已更改并持久化: $dir');
  }

  /// 恢复默认转写盘位置（桌面 `translate results`），并清除持久化值。
  Future<void> resetTranscriptDir() async {
    final def = _bridge.transcriptDir();
    if (def.isEmpty) return;
    _transcriptPath = def;
    notifyListeners();
    final prefs = await SharedPreferences.getInstance();
    await prefs.remove(_kTranscriptDirKey);
    debugPrint('[transcript] 已恢复默认位置: $def');
  }

  TranscriptStats get transcriptStats => _transcriptStats;
  String get sessionError => _sessionError;
  bool get bootstrapped => _bootstrapped;

  /// 是否暂停中
  bool get paused => _paused;

  /// FFI 桥。页面需要直接调会话级 API（词库导入、模型导入）时用它。
  Bridge get bridge => _bridge;

  /// 已从转写盘加载的更早字幕（旧 → 新）
  List<SubtitleLine> get history => List.unmodifiable(_history);
  bool get loadingHistory => _loadingHistory;
  bool get hasMoreHistory => _hasMoreHistory;

  /// 自检是否全绿
  bool get selfcheckAllOk => _steps.isNotEmpty && _steps.every((s) => s.ok);

  // ── 窗口与托盘 ──
  /// 点 ✕ 时是否最小化到托盘（否则直接退出）。
  /// 设置项「最小化到托盘」，默认开。
  /// 关闭窗口行为：'ask' | 'minimize' | 'quit'
  String get closeAction => _closeAction;

  /// 「是否最小化到托盘」—— 由 `closeAction` 派生，供托盘初始化读取。
  bool get minimizeToTray => _closeAction == 'minimize';

  /// 设置关闭窗口行为（持久化）。
  Future<void> setCloseAction(String action) async {
    if (!const {'ask', 'minimize', 'quit'}.contains(action)) return;
    if (_closeAction == action) return;
    _closeAction = action;
    notifyListeners();
    final prefs = await SharedPreferences.getInstance();
    await prefs.setString(_kCloseActionKey, action);
    debugPrint('[close] 行为已设置为 $action');
  }

  /// 字幕是否开启 —— **唯一真相源 = `lyricsWindowEnabled`**（独立字幕窗）。
  ///
  /// ⚠️ 2026-10-25 修「托盘打开/关闭字幕失效」：旧实现维护了一个独立
  /// 布尔 `_subtitlesVisible`，托盘「字幕」走的是隐藏/显示**主窗口**
  /// （旧悬浮字幕语义），与用户实际使用的独立字幕窗（第二实例进程）
  /// 完全无关 —— 点了没反应。现在派生自 `lyricsWindowEnabled`，
  /// 托盘与设置页开关指向同一个状态，天然同步。
  bool get subtitlesVisible => _lyricsWindowEnabled;

  /// 开关字幕窗（托盘菜单「打开/关闭字幕」与设置页共用，幂等）。
  ///
  /// 开 = 启动第二实例；关 = 杀进程 + 清心跳 + 删共享文件。
  /// 字幕窗进程自己退出（用户点字幕窗 ✕ / 崩溃）时 exitCode 回调
  /// 把本标志置回 false，托盘菜单文案下次刷新同步。
  Future<void> toggleSubtitlesWindow() async {
    await setLyricsWindowEnabled(!_lyricsWindowEnabled);
    // 托盘菜单文案（打开/关闭字幕）随状态刷新
    await TrayService.instance.refreshMenu();
  }

  // ── 悬浮字幕窗 ──
  /// 是否处于悬浮字幕模式（置顶 + 无边框 + 可拖动 + 半透明底）。
  bool get floatingMode => _floatingMode;

  /// 正文字号（悬浮字幕可调，16~40）。
  double get subtitleFontSize => _subtitleFontSize;

  /// 切换悬浮字幕模式。
  Future<void> setFloatingMode(bool v) async {
    if (_floatingMode == v) return;
    _floatingMode = v;
    notifyListeners();
    await setFloatingSubtitleMode(v);
  }

  /// 设置正文字号（夹在 16~40）。
  void setSubtitleFontSize(double v) {
    final clamped = v.clamp(16.0, 40.0);
    if (_subtitleFontSize == clamped) return;
    _subtitleFontSize = clamped;
    notifyListeners();
    // 字号变化要即时反映到歌词窗（歌词窗字号复用主窗口字号）
    _writeLyricsShared();
  }

  /// 是否启用独立歌词窗（设置页开关）。
  bool get lyricsWindowEnabled => _lyricsWindowEnabled;

  /// 启用/关闭独立歌词窗。
  ///
  /// 开 → 启动 `translator.exe --lyrics` 第二实例；
  /// 关 → 杀掉第二实例进程并清空共享文件。
  ///
  /// 任何一步失败只记日志：歌词窗是增强功能，不该影响主转写。
  Future<void> setLyricsWindowEnabled(bool v) async {
    if (_lyricsWindowEnabled == v) return;
    _lyricsWindowEnabled = v;
    notifyListeners();
    if (AndroidChannel.instance.isAndroid) {
      // Android：原生 Overlay 字幕窗（任务 #24）。
      // 未授予悬浮窗权限时先跳系统设置页引导，本次不强制打开
      // （用户授权后回来再开一次即可）—— 禁止静默失败。
      if (v) {
        if (!await AndroidChannel.instance.overlayPermission()) {
          await AndroidChannel.instance.requestOverlayPermission();
          _sessionError = '请在系统设置中允许 Translator 显示其他应用上方内容，再回来重新开启字幕';
          _lyricsWindowEnabled = false;
          notifyListeners();
          return;
        }
        final err = await AndroidChannel.instance.showSubtitle();
        if (err != null) {
          _sessionError = '字幕窗启动失败：$err';
          _lyricsWindowEnabled = false;
          notifyListeners();
          return;
        }
        _writeLyricsShared(); // 立即喂一帧，避免开窗空白
      } else {
        await AndroidChannel.instance.hideSubtitle();
      }
      return;
    }
    if (v) {
      unawaited(_startLyricsProcess());
    } else {
      _stopLyricsProcess();
    }
  }

  /// 启动歌词窗第二实例（同目录的 translator.exe，带 TRANSLATOR_LYRICS=1）。
  ///
  /// `Process.start` 返回 `Future<Process>` —— 必须 await 后才拿到 Process，
  /// 才能读 pid / 监听 exitCode。方法改成 async。
  Future<void> _startLyricsProcess() async {
    if (!Platform.isWindows) {
      debugPrint('[lyrics] 字幕窗目前仅支持 Windows');
      return;
    }
    if (_lyricsProcess != null) return;
    try {
      // 主程序自身的 exe 路径 = 第二实例的路径（同一安装目录）。
      // 用环境变量标记歌词模式（见 lyrics_window.isLyricsMode），
      // 不传命令行参数 —— Windows 下参数解析不可靠。
      final exe = Platform.resolvedExecutable;
      // ⚠️ 不能用 detachedWithStdio：该模式禁止访问 exitCode/pid
      // （抛 "Bad state: Process is detached"，曾导致字幕窗被误报
      // 「启动失败」且拿不到退出码）。normal 模式两者都可用；
      // 父进程退出后子存活的孤儿场景由字幕窗自己的 60s 自愈兜底。
      final proc = await Process.start(
        exe,
        const <String>[],
        environment: {'TRANSLATOR_LYRICS': '1'},
      );
      _lyricsProcess = proc;
      unawaited(proc.exitCode.then((code) {
        _lyricsExitCode = code;
        // 进程退出（用户关窗 / 崩溃）：清引用，不重启（避免崩溃循环）
        if (_lyricsProcess == proc) {
          _lyricsProcess = null;
          debugPrint('[lyrics] 字幕窗进程退出（code=$code）');
          // ⚠️ 2026-10-25 修「字幕窗关了，设置里开关还显示开着」：
          // 进程是**自己**退出的（用户点字幕窗关闭 / 崩溃 / 判死自杀），
          // 不是走 setLyricsWindowEnabled(false) 那条路 —— 那条路已经
          // 把 _lyricsWindowEnabled 置 false 了。这里只在它仍为 true 时
          // 同步关掉开关，让设置页反映真实状态。
          // 递归安全：置 false 后再走 _stopLyricsProcess 时 _lyricsProcess
          // 已是 null，kill 空操作。
          if (_lyricsWindowEnabled) {
            _lyricsWindowEnabled = false;
            _lyricsHeartbeat?.cancel();
            _lyricsHeartbeat = null;
            notifyListeners();
          }
        }
      }));
      debugPrint('[lyrics] 字幕窗第二实例已启动 PID=${proc.pid}');
      // 立即喂一次当前字幕，让歌词窗一打开就有内容
      _writeLyricsShared();
      // 心跳：每 2s 刷新共享文件 mtime，保证主程序存活期间
      // 歌词窗（靠 mtime 判活）不会误判为「主程序没了」。
      _lyricsHeartbeat?.cancel();
      _lyricsHeartbeat = Timer.periodic(
        const Duration(seconds: 2),
        (_) => _writeLyricsShared(),
      );
    } catch (e) {
      _lyricsProcess = null;
      debugPrint('[lyrics] 启动字幕窗失败: $e');
    }
  }

  /// 停止歌词窗第二实例并清空共享文件。
  void _stopLyricsProcess() {
    _lyricsHeartbeat?.cancel();
    _lyricsHeartbeat = null;
    try {
      _lyricsProcess?.kill();
    } catch (e) {
      debugPrint('[lyrics] 停止字幕窗失败（忽略）: $e');
    }
    _lyricsProcess = null;
    try {
      final f = File(lyricsSharedPath());
      if (f.existsSync()) f.deleteSync();
    } catch (_) {}
  }

  /// 把当前字幕行 + 字号写进共享文件（歌词窗每 100ms 轮询读）。
  ///
  /// 只在主实例里调用（歌词窗实例不写）。转写中每 100ms 的 _poll
  /// 若字幕有变化就刷新 —— 文件停更即表示主实例没了，歌词窗据此隐藏。
  /// 字幕窗共享内容的行数上限（用户方案：到达上限自动滚动，旧内容丢弃）。
  static const _lyricsWindowMaxLines = 80;

  void _writeLyricsShared() {
    if (!_lyricsWindowEnabled) return;
    // 只传最近 N 行锁定行 —— 超出部分由字幕窗滚动丢弃（用户定案）。
    final tail = _lines.length > _lyricsWindowMaxLines
        ? _lines.sublist(_lines.length - _lyricsWindowMaxLines)
        : _lines;
    final lines = <Map<String, dynamic>>[
      for (final l in tail)
        {
          's': l.source,
          if (l.target != null) 't': l.target,
          if (l.speaker != null && l.speaker!.isNotEmpty) 'sp': l.speaker,
        },
    ];
    // partial 一并下发：字幕窗流式形态（一句不切，当前行实时刷新）
    if (AndroidChannel.instance.isAndroid) {
      // Android 走原生 Overlay（任务 #24）：MethodChannel 直推，
      // 不写共享文件（那是桌面第二实例的通道，Android 用不了）。
      // 用户定版：固定两行 —— 原文流式行与译文流式行**各自独立**
      // 顶替对应行（不做合并顶替，否则中文流式会把英文行整行吃掉）。
      // fire-and-forget：字幕推送失败绝不阻塞转写轮询。
      unawaited(AndroidChannel.instance.updateSubtitle(
        lines: [
          for (final l in tail)
            {'s': l.source, 't': l.target ?? ''},
        ],
        partial: _partial,
        partialZh: _partialZh,
      ));
      return;
    }
    writeLyricsShared(
        fontSize: _subtitleFontSize,
        lines: lines,
        partial: _partial,
        partialZh: _partialZh);
  }

  /// 切换「最小化到托盘」并即时生效。
  ///
  /// 兼容旧调用方：映射到 `closeAction`（true → 'minimize'，false → 'quit'）。
  Future<void> setMinimizeToTray(bool v) async {
    await setCloseAction(v ? 'minimize' : 'quit');
    await TrayService.instance.setMinimizeToTray(v);
  }

  /// 是否预发布版本。
  ///
  /// 判据是**语义版本主版本号 < 1**（`v0.x.y` 一律算测试版）——
  /// 而不是看渠道名：渠道可以叫 `release` 但版本仍是 0.x。
  /// 主版本号进 1 才算正式发布（用户规定的递进规则：
  /// 正式版大迭代更新全部数字 v1.*.* / v2.*.*，测试版前面恒为 0）。
  bool get isPrerelease {
    final m = RegExp(r'-v(\d+)\.\d+\.\d+-').firstMatch(_bridge.version());
    if (m == null) return false;
    return int.tryParse(m.group(1) ?? '') == 0;
  }

  /// 是否运行在卓易通容器内
  bool get isZhuoYiTong => _runtime == 'zhuoyitong';

  /// 点 ✕ 时是否「最小化到通知栏」而不是退出 —— 派生自 `_closeAction`，
  /// 仅为兼容 TrayService.init 的旧参数保留。
  /// 关闭窗口时的行为（2026-10-25 用户要求「参考大厂，让用户选择」）。
  ///
  /// - `'ask'`      每次关闭都弹窗询问（**默认**，大厂标准做法）
  /// - `'minimize'` 直接最小化到通知栏，继续转写
  /// - `'quit'`     直接走退出流程（保存后退出）
  ///
  /// 持久化于 shared_preferences，重启后仍生效。
  String _closeAction = 'ask';

  /// 持久化 key：关闭窗口行为
  static const _kCloseActionKey = 'close_action';
  bool _floatingMode = false;
  double _subtitleFontSize = 22.0;

  // ── 独立歌词窗（第二实例）──
  /// 是否启用独立歌词窗（QQ 音乐桌面歌词风格，与主窗口并存）。
  bool _lyricsWindowEnabled = false;

  /// 歌词窗第二实例进程（`translator.exe --lyrics`）。
  ///
  /// 用第二实例而非多窗口插件：零依赖、不碰 C++/pubspec/窗口管理器，
  /// 崩溃与主实例隔离。主实例负责启动/停止它，并通过共享文件喂字幕。
  Process? _lyricsProcess;

  /// 最近一次字幕窗进程的退出码（自检取证：非 0=崩溃，0=正常退出）
  int? _lyricsExitCode;

  /// 歌词窗心跳定时器。
  ///
  /// 歌词窗靠「共享文件 mtime」判断主实例是否还活着（>5s 未更新即隐藏）。
  /// 但「停止转写」≠「主程序退出」—— 转写停了歌词窗仍应保留最后内容。
  /// 故用**独立心跳**（与转写轮询解耦）：只要歌词窗开着就每 2s 刷新一次文件
  /// （即使内容没变），保证主程序存活期间歌词窗不会误判为「主程序没了」。
  Timer? _lyricsHeartbeat;

  /// 切换运行时。卓易通需重新探测能力 —— 容器限制与原生不同。
  void setRuntime(String v) {
    if (_runtime == v) return;
    _runtime = v;
    notifyListeners();
    probeCapability();
  }

  void setProfile(String v) {
    if (_profile == v) return;
    _profile = v;
    notifyListeners();
  }

  /// 待翻译的行数（MT 背压指示）
  int get translatingCount => _lines.where((l) => l.isTranslating).length;

  /// 需复核的行数（哨兵校验失败）
  int get reviewCount => _lines.where((l) => l.needsReview).length;

  // ── 操作 ──

  void setTab(int i) {
    if (_tabIndex == i) return;
    _tabIndex = i;
    notifyListeners();
  }

  /// 探测 Android 上的模型布局（容错版）。
  ///
  /// ## 为什么必须容错
  ///
  /// `Directory.existsSync()` / `listSync()` 在**目录无权访问**时抛
  /// `FileSystemException`（errno 13），而不是返回 false / 空列表。
  /// Android 11+ 分区存储下，若模型被 `adb push` 到外部目录且属主是 shell，
  /// 应用探测时就会抛这个异常 → **中断整个 bootstrap**（界面永远停在
  /// 「加载中」，且日志里只有一条 platform error）。
  ///
  /// 故所有目录访问都包 try/catch，探不到就当「没有」，继续下一个落点。
  ///
  /// ## 布局约定
  ///
  /// ```
  /// <root>/models/asr/<模型名>/{encoder.int8.onnx, tokens.txt, ...}
  /// <root>/models/mt/{Bergamot 权重}          ← 直接一层
  /// ```
  ///
  /// Rust 的 `probe_asr_dir` **只看本层**（要能直接命中 encoder*.onnx），
  /// 而模型实际在下一层 → 这里必须定位到**具体模型目录**。
  ModelPaths _probeAndroidModels(String root) {
    // ASR：asr 目录下第一个含 encoder*.onnx 的子目录
    var asrDir = '';
    final asrRoot = Directory('$root${Platform.pathSeparator}asr');
    try {
      if (asrRoot.existsSync()) {
        final subs = <Directory>[];
        try {
          subs.addAll(asrRoot.listSync().whereType<Directory>());
        } catch (_) {
          // 该子目录不可读 → 视为空
        }
        for (final sub in subs) {
          bool hasEncoder;
          try {
            hasEncoder = sub.listSync().any((f) =>
                f is File &&
                f.path
                    .split(Platform.pathSeparator)
                    .last
                    .startsWith('encoder') &&
                f.path.endsWith('.onnx'));
          } catch (_) {
            hasEncoder = false;
          }
          if (hasEncoder) {
            asrDir = sub.path;
            break;
          }
        }
        // 兜底：模型文件直接放在 asr 目录下
        if (asrDir.isEmpty) {
          try {
            if (asrRoot.listSync().any(
                (f) => f is File && f.path.endsWith('encoder.int8.onnx'))) {
              asrDir = asrRoot.path;
            }
          } catch (_) {
            // 不可读 → 保持空
          }
        }
      }
    } catch (_) {
      // asr 根目录不可访问（如外部目录属主是 shell）
    }

    // MT 权重在 models/mt/ 直接一层（Bergamot 格式）
    var mtDir = '';
    final mtPath = '$root${Platform.pathSeparator}mt';
    try {
      if (Directory(mtPath).existsSync()) mtDir = mtPath;
    } catch (_) {
      // 不可访问 → 视为无 MT
    }

    return ModelPaths(asrDir, mtDir, root);
  }

  /// 启动引导：加载库 → 建会话 → 自检 → 探测采集能力。
  ///
  /// 在 `HomeShell.initState` 里调用（不能放构造函数 ——
  /// FFI 抛异常会让整个 App 起不来，连错误页都渲染不了）。
  ///
  /// 四种失败路径都会在 UI 上呈现，不崩溃：
  /// 库加载失败 / 模型缺失 / 词库语法错 / 采集不可用。
  Future<void> bootstrap() async {
    // 1) 转写盘位置：用户改过的（持久化）优先；否则用默认位置
    //    （桌面 translate results）
    final prefs = await SharedPreferences.getInstance();
    final savedDir = prefs.getString(_kTranscriptDirKey);
    if (savedDir != null && savedDir.isNotEmpty) {
      _transcriptPath = savedDir;
    } else if (AndroidChannel.instance.isAndroid) {
      // Android 没有「桌面」概念，Rust 的 default_transcript_dir 会解析到
      // $HOME/Desktop（通常不存在/不可写）→ 改用应用外部私有目录。
      final ext = await AndroidChannel.instance.externalFilesDir();
      _transcriptPath =
          ext.isEmpty ? '' : '$ext${Platform.pathSeparator}translate results';
    } else {
      _transcriptPath = _bridge.transcriptDir();
    } // 关闭窗口行为（默认 'ask' = 每次询问，大厂标准做法）
    final savedAction = prefs.getString(_kCloseActionKey);
    if (savedAction != null &&
        const {'ask', 'minimize', 'quit'}.contains(savedAction)) {
      _closeAction = savedAction;
    }

    // 2) 建会话（毫秒返回，不加载引擎）
    // 声源配置：用户勾了几个就建几个采集器
    final sources = <Map<String, dynamic>>[];
    if (_sources.contains('systemAudio')) {
      sources.add({'kind': 'systemAudio', 'speaker': _speakerRemote});
    }
    if (_sources.contains('microphone')) {
      sources.add({'kind': 'microphone', 'speaker': _speakerLocal});
    }
    if (sources.isEmpty) {
      sources.add({'kind': 'systemAudio', 'speaker': _speakerRemote});
    }

    // ── 线程调整器（2026-10-28 任务 #23）──
    // Android 按硬件画像推导默认线程档，**必须在 cfg 构建之前**：
    //
    // ⚠️ 第一版写在下面的模型探测分支里，而 cfg 在它之前已把
    // _asrThreads 读走冻结 —— 真机日志实锤：画像算出 asr=6，
    // Rust 侧 ASR 加载日志却是 threads=4（档位根本没传下去）。
    // 顺序即契约：改状态的代码永远放在消费它之前。
    //
    // 产品定位（MEMORY 2026-10-25）：默认**满档**保字幕跟手，
    // 只有硬件真实受限才收 —— 与 Windows 端「不因检测到会议软件
    // 而降档」同理。
    if (AndroidChannel.instance.isAndroid) {
      final prof = await AndroidChannel.instance.deviceProfile();
      setAsrThreads(prof.defaultAsrThreads);
      debugPrint('[threads] Android 画像 $prof → asr=$_asrThreads');
    }

    // 资源档位：决定占用多少 CPU 线程（轻量 1 / 均衡 2 / 全力 4）。
    // Rust 侧收到后会自动联动线程数、断句档与翻译精度档。
    final cfg = <String, dynamic>{
      'resource_tier': _resourceTier,
      'profile': _profile == 'fast' ? 'fast' : 'stable',
      // 线程数由 Slider 直接控制（Rust 侧 asr_threads 后解析，优先级高于
      // resource_tier 的联动值）—— 这是 UI 的真实入口。
      'asr_threads': _asrThreads,
      'mt_threads': _mtThreads,
      'memory_window_lines': 2000,
      'sources': sources,
    };

    // 模型目录：安装态在 {app}\models\，开发态在 translator_core\models\。
    // 找不到就用 mock 后端跑通链路（而不是让整个会话创建失败）。
    //
    // Android 例外：Rust 的 discover_model_layout 探的是「exe 相邻目录」，
    // 而 Android 上 current_exe 是 .so 所在目录（app/lib/arm64），
    // 探不到 —— 改用应用外部私有目录（adb push 目标）：
    //   <externalFilesDir>/models/{asr,mt,offline,glossary}
    if (AndroidChannel.instance.isAndroid) {
      // ── 模型内置化（2026-10-28 任务 #17）──
      // 模型随 APK 打进 assets，首启由 Kotlin 解压到 filesDir/models。
      // 探测前**必须等解压完成** —— 否则探到半成品目录，加载必崩。
      // 已解压过的启动第一次轮询即返回 ready，几乎零耗时。
      final ready = await AndroidChannel.instance.waitForModels(
        onTick: (hint) => debugPrint('[models] $hint'),
      );
      if (!ready) {
        debugPrint('[models] Android 模型解压超时/失败，退回 mock 后端');
      }
      // ⚠️ 2026-10-27：Android 上要**依次试两个落点**。
      //
      // 1) 内部私有目录 /data/data/<pkg>/files —— **首选**。
      //    权限天然正确（属主=应用 UID），模型由 `run-as` 搬运进来。
      // 2) 外部私有目录 /storage/emulated/0/Android/data/<pkg>/files —— 兜底。
      //    ⚠️ 这里 `existsSync()` 在**无权访问**时会抛 FileSystemException
      //    （errno 13），而不是返回 false → 整个 bootstrap 被中断。
      //    所以目录探测必须包 try/catch，且两个落点要独立探测。
      final roots = <String>[];
      final internal = await AndroidChannel.instance.internalFilesDir();
      if (internal.isNotEmpty) roots.add('$internal/models');
      final external = await AndroidChannel.instance.externalFilesDir();
      if (external.isNotEmpty) roots.add('$external/models');

      models = const ModelPaths('', '', '');
      for (final root in roots) {
        final found = _probeAndroidModels(root);
        if (found.asr.isNotEmpty || found.mt.isNotEmpty) {
          models = found;
          debugPrint('[models] Android 模型目录命中: $root');
          break;
        }
      }
      if (models.asr.isEmpty) {
        debugPrint('[models] Android 未找到模型（探测过 ${roots.length} 个落点）');
      }
    } else {
      models = _bridge.discoverModels();
    }
    if (models.asr.isNotEmpty) {
      cfg['asr_model_dir'] = models.asr;
      cfg['backend'] = 'auto';
      if (models.mt.isNotEmpty) {
        cfg['mt_model_dir'] = models.mt;
      } else {
        // MT 权重未就位 → 翻译**可选**（Rust 侧降级为「仅识别」模式，
        // mt_ready=0 如实上报，UI 明示「翻译引擎未启用」）。
        // 阶段1（Android 只装 ASR 模型）走的就是这条路径。
        cfg['mt_optional'] = true;
        debugPrint('[models] 未找到 MT 权重 → 仅识别模式（mt_optional）');
      }
      // 两遍解码第二遍的离线模型（SenseVoice，成行后重识别修正文本）。
      // 目录缺失时 Rust 侧安静失败并自动退回单遍流式，不影响使用。
      cfg['offline_model_dir'] =
          '${models.root}${Platform.pathSeparator}offline'
          '${Platform.pathSeparator}sense-voice-int8';
      // 讲师课程术语库（随 models 一起安装，加载后 ASR 纠错 + MT 术语
      // 替换同时生效）。来源：学习空间讲师笔记（SIM 经济学 Ivan Soh）。
      // 用户也可在词库页导入自己的 TSV 覆盖/追加。
      final glossary = File('${models.root}${Platform.pathSeparator}glossary'
          '${Platform.pathSeparator}roger-courses.tsv');
      if (glossary.existsSync()) {
        cfg['glossary_paths'] = [glossary.path];
        debugPrint('[glossary] 默认词库: ${glossary.path}');
      }
    } else {
      // 连 ASR 模型都没有 → mock 后端跑通链路（而不是让会话创建失败）
      cfg['backend'] = 'mock';
    }
    if (_transcriptPath.isNotEmpty) {
      cfg['transcript_dir'] = _transcriptPath;
    }
    final ok = _bridge.sessionCreate(
      jsonEncode(cfg),
      _source == 'microphone' ? 1 : 0,
      0,
    );
    if (!ok) {
      _sessionError = '会话创建失败：无法打开音频采集器。\n'
          '可能原因：无音频设备、设备被独占、或系统策略限制。';
      _bootstrapped = true; // 标记已完成，UI 不再显示"加载中"
      notifyListeners();
      return;
    }

    // 3) 自检（5 步，毫秒级）
    _steps = _bridge.selfcheck();

    // 4) 探测采集能力 —— 现在是真的在问系统
    await probeCapability();

    // 5) 会话状态
    _status = _bridge.sessionStatus();

    _bootstrapped = true;
    notifyListeners();
    _dumpRuntimeDiag('bootstrap'); // 开机即落盘，未点开始也能读采集参数
  }

  /// 探测采集能力。启动时调用，结果决定是否显示降级横幅。
  Future<void> probeCapability() async {
    try {
      final result = await _bridge.probeCapture();
      _capability = switch (result.kind) {
        CaptureKind.systemAudio => const CaptureCapability.system(),
        CaptureKind.microphone =>
          CaptureCapability.mic(result.reason ?? '未知原因'),
        CaptureKind.unavailable =>
          CaptureCapability.unavailable(result.reason ?? '未知原因'),
      };
    } catch (e) {
      _capability = CaptureCapability.unavailable('$e');
    }
    notifyListeners();
  }

  /// 开始转写。
  ///
  /// EN 先于 ZH 显示：翻译慢时英文立即上屏，译文后补。
  /// 这是 MT 背压策略（DESIGN §6.2）。
  Future<void> start() async {
    if (_running) return;
    if (!_bridge.sessionStarted) {
      _sessionError = '会话尚未启动，请先完成启动自检';
      debugPrint('[transcribe] start 失败：会话未启动');
      notifyListeners();
      return;
    }

    final rc = _bridge.sessionStart();
    if (rc != 0) {
      _sessionError = '会话启动失败（错误码 $rc）';
      debugPrint('[transcribe] start 失败：sessionStart rc=$rc');
      notifyListeners();
      return;
    }

    // Android：起 Kotlin 采集服务（生产者）。
    //
    // 顺序要点：会话先起（Rust 侧采集器就位、running 置真），
    // 再让 Kotlin 推音频 —— 反了的话推入会被 running 校验拒掉。
    if (AndroidChannel.instance.isAndroid) {
      // ── 双路并行采集（2026-10-28 用户实测「麦克风收音不到」后改造）──
      //
      // 旧逻辑：只起**一路**。默认声源集含 systemAudio + microphone，
      // 走内放时麦克风那路根本没采 → 用户对着手机说话没反应。
      // 这不是设置问题，是架构缺口：会话建了两路采集器，生产者却只起一路。
      //
      // 新逻辑：两路各自查下标、各自传给 Kotlin，并行跑。
      //   kind 0 = 系统内放（会议声音，会议软件是**被翻译对象**）
      //   kind 1 = 麦克风（本地说话）
      //
      // 下标必须**按声源类型**查，不能写死 0：会话的采集器列表只含
      // 创建成功的，下标与 cfg.sources 可能错位（容器内内放建不起来
      // 就只剩麦克风在下标 0）。
      const kindSystemAudio = 0;
      const kindMicrophone = 1;

      // 用户勾了哪路就起哪路；未勾的传 -1（该路不起）。
      final wantSys = _sources.contains('systemAudio');
      final wantMic = _sources.contains('microphone');
      final sysIdx = wantSys ? _bridge.captureIndex(kindSystemAudio) : -1;
      final micIdx = wantMic ? _bridge.captureIndex(kindMicrophone) : -1;

      debugPrint(
          '[transcribe] Android 双路采集：wantSys=$wantSys(sys=$sysIdx) '
          'wantMic=$wantMic(mic=$micIdx)');

      if (sysIdx < 0 && micIdx < 0) {
        _sessionError = '会话未创建任何采集器，无法采集音频';
        debugPrint('[transcribe] Android 无可用采集器');
        _bridge.sessionStop();
        notifyListeners();
        return;
      }
      final err = await AndroidChannel.instance.startCapture(
        session: _bridge.sessionAddress,
        sysIndex: sysIdx,
        micIndex: micIdx,
        useSystemAudio: wantSys && sysIdx >= 0,
      );
      if (err != null) {
        _sessionError = '采集启动失败：$err';
        debugPrint('[transcribe] Android 采集启动失败：$err');
        // 回滚：Rust 侧会话已起但没有生产者，停掉避免空转
        _bridge.sessionStop();
        notifyListeners();
        return;
      }
      // 采集状态：投屏授权是**异步**的（用户在系统弹窗点「立即开始」
      // 服务才真正起来），启动后立刻查拿到的是旧状态 —— 必须轮询。
      // 后台等待，不阻塞「转写已启动」的 UI 反馈。
      unawaited(_awaitAndroidCaptureStatus());
    }

    debugPrint('[transcribe] 转写已启动');
    _running = true;
    _dumpRuntimeDiag('start'); // 落盘启动现场（running 已置真）
    _lines = [];
    _history.clear();
    _hasMoreHistory = true;
    _partial = '';
    notifyListeners();
    // 托盘菜单「开始转写」→「停止转写」
    TrayService.instance.refreshMenu();

    // 轮询字幕事件。音频不经过 Dart —— 引擎线程自己消费，
    // 这里只取"已经产出"的字幕。
    _pollTimer?.cancel();
    _pollTimer = Timer.periodic(
      // 60ms（2026-10-24 降延迟）：takeEvents/status/partial 都是轻量
      // mutex 读，主窗口流式双语上屏更跟手。
      const Duration(milliseconds: 60),
      (_) => _poll(),
    );
  }

  /// 后台等待 Android 采集服务真正起来，并把真实模式/降级原因呈现出来。
  ///
  /// ## 为什么必须轮询（不能查一次）
  ///
  /// 投屏授权是**异步**的：`startCapture` 只回「已受理」，采集服务要等
  /// 用户在系统弹窗点「立即开始」才真正起来。启动后立刻查 `captureStatus`
  /// 拿到的是**旧状态**（idle 或上一次的模式），于是：
  /// - 用户拒绝投屏 → 看不到「已降级为麦克风」，以为没在采
  /// - 授权成功 → 模式仍是旧的，UI 显示不对
  ///
  /// 后台轮询直到模式变为非 idle，把真实状态与降级原因写回
  /// [_sessionError]（**显式呈现，禁止静默降级**，DESIGN §2.5.2）。
  Future<void> _awaitAndroidCaptureStatus() async {
    final st = await AndroidChannel.instance.waitForCaptureMode();
    if (!_running) return; // 期间用户已停止 → 不再改状态
    debugPrint('[transcribe] Android 采集模式=${st.mode} 降级=${st.degrade}');
    if (st.degraded) {
      _sessionError = st.degrade;
      notifyListeners();
    }
  }

  /// 停止转写并冲刷残留段。
  ///
  /// 冲刷出的字幕由轮询一次性取回，
  /// 故此处无需返回值 —— 避免两条路径重复入列。
  /// 暂停转写。
  ///
  /// **可恢复**：保留会话与音频接收端，恢复后接着转，
  /// 字幕不会断、seq 连续、转写盘是同一个文件。
  ///
  /// 暂停期间引擎不识别新音频，但**在途译文仍会处理** ——
  /// 不会出现「暂停很久后译文一次性冒出来」。
  Future<void> pause() async {
    if (!_running || _paused) return;
    _paused = true;
    _bridge.sessionPause(true);
    notifyListeners();
  }

  /// 恢复转写
  Future<void> resume() async {
    if (!_paused) return;
    _paused = false;
    _bridge.sessionPause(false);
    notifyListeners();
  }

  /// 暂停/恢复切换（UI 只有一个按钮时用）
  Future<void> togglePause() => _paused ? resume() : pause();

  Future<void> stop() async {
    if (!_running) return;
    _running = false;
    _paused = false;
    _pollTimer?.cancel();
    _pollTimer = null;

    // Android：先停 Kotlin 生产者，再停 Rust 会话。
    //
    // ⚠️ 顺序不可颠倒：Kotlin 的采集线程持有会话指针，
    // 会话先释放 → 线程还在推 → use-after-free（必崩）。
    // stopCapture 回包前 Kotlin 已确保线程退出。
    if (AndroidChannel.instance.isAndroid) {
      await AndroidChannel.instance.stopCapture();
    }

    // 同步冲刷最后一句的译文 —— 用户说了却没翻的那半句要补齐
    _bridge.sessionStop();
    _poll(); // 收尾再取一次，拿到冲刷出的最后几行
    notifyListeners();
  }

  /// 从转写盘加载更早的字幕（向上滚动时触发）。
  ///
  /// 这就是「内存窗口丢弃最初内容」后还能找回的机制：
  /// 内存只留最近 N 行，更早的从 `transcript-*.dsht` 按 seq 分页读。
  ///
  /// 去重按 `startMs + source` —— 同一行在内存与文件里都存在时，
  /// 以内存版为准（它的译文可能更新）。
  Future<void> loadMoreHistory() async {
    if (_loadingHistory || !_hasMoreHistory) return;
    if (_bridge.sessionStarted == false) return;

    _loadingHistory = true;
    notifyListeners();

    // 每次往前取 50 行
    const pageSize = 50;
    // 已经加载到多早：_history 最早一行的 startMs，或当前内存最早一行的
    final earliestKnown = _history.isNotEmpty
        ? _history.first.startMs
        : (_lines.isNotEmpty ? _lines.first.startMs : 0);

    // 从比 earliestKnown 更早的位置取。用 last_seq 反推不可靠，
    // 故从 0 开始扫并由调用方过滤 —— 50 行的小分页代价可接受。
    final older = _bridge.transcriptQuery(0, pageSize * 4);
    _loadingHistory = false;

    // 过滤掉已知的（按 startMs+source 去重）
    final known = <String>{
      for (final l in _lines) '${l.startMs}|${l.source}',
      for (final l in _history) '${l.startMs}|${l.source}',
    };
    final fresh = older
        .where((l) =>
            l.startMs < earliestKnown &&
            !known.contains('${l.startMs}|${l.source}'))
        .toList()
      ..sort((a, b) => a.startMs.compareTo(b.startMs));

    if (fresh.isEmpty) {
      _hasMoreHistory = false;
    } else {
      _history.insertAll(0, fresh);
      // 内存窗口只保留最近的，其余交给 history
      _hasMoreHistory = older.length >= pageSize;
    }
    notifyListeners();
  }

  /// 轮询：取增量事件 → 更新列表 / partial / 状态
  void _poll() {
    final events = _bridge.takeEvents();
    if (events.isNotEmpty) {
      debugPrint('[poll] 收到 ${events.length} 个事件');
    }
    var changed = false;

    for (final e in events) {
      switch (e.kind) {
        case 'add':
          _lines.add(SubtitleLine(
            seq: e.seq,
            source: e.source ?? '',
            target: null,
            startMs: e.startMs,
            endMs: e.endMs,
            needsReview: e.needsReview,
            forcedCut: e.forcedCut,
            speaker: e.speaker,
          ));
          _partial = '';
          _partialZh = '';
          changed = true;
        case 'fill':
          // 译文回填：⚠️ 必须按 **seq** 匹配（2026-10-24 定位：
          // fill 事件不带 start_ms，旧代码用 l.startMs==e.startMs，
          // 而 e.startMs 恒为 0 → 只有第一句（startMs 恰好=0）能回填，
          // 其余译文全部落空 —— 用户看到的「翻译没出来」。
          final idx =
              _lines.indexWhere((l) => l.seq == e.seq && l.target == null);
          if (idx >= 0) {
            final old = _lines[idx];
            _lines[idx] = SubtitleLine(
              seq: old.seq,
              source: old.source,
              target: e.target,
              startMs: old.startMs,
              endMs: old.endMs,
              needsReview: e.needsReview,
              forcedCut: old.forcedCut,
              speaker: old.speaker,
            );
            changed = true;
          }
        case 'rev':
          final idx =
              _lines.indexWhere((l) => l.seq == e.seq && l.target == null);
          if (idx >= 0) {
            final old = _lines[idx];
            _lines[idx] = SubtitleLine(
              seq: old.seq,
              source: old.source,
              target: old.target,
              startMs: old.startMs,
              endMs: old.endMs,
              needsReview: true,
              forcedCut: old.forcedCut,
              speaker: old.speaker,
            );
            changed = true;
          }
      }
    }

    // ⚠️ partial 变化必须置 changed（2026-10-24 用户实测「没有流式输出」
    // 的第二处原因）：旧代码只更新了 _partial 却没标记变化，共享文件
    // 就不会重写 —— 字幕窗（悬浮字幕）收不到流式文本，看着像没输出。
    final prevPartial = _partial;
    final prevZh = _partialZh;
    _partial = _bridge.sessionPartial();
    _partialZh = _bridge.sessionPartialZh(); // ★ 流式译文同帧读取
    if (_partial != prevPartial || _partialZh != prevZh) {
      changed = true;
    }
    final st = _bridge.sessionStatus();
    if (st != _status) {
      _status = st;
      changed = true;
    }
    if (_transcriptStats != _bridge.transcriptStats()) {
      _transcriptStats = _bridge.transcriptStats();
      changed = true;
    }

    if (changed) {
      notifyListeners();
      // 字幕/状态有变化 → 刷新歌词窗共享文件（主实例的"心跳"，
      // 歌词窗靠文件修改时间判断主实例是否还活着）
      _writeLyricsShared();
    }

    // 运行时诊断落盘（每 ~2 秒）。用户跑一次后读这个文件即可定位
    // 「转写不出字」断在哪一环：capture_rms 低=采集无声，
    // vad_segments=0=VAD 没切段，asr_finals=0=模型没出字。
    _diagTick++;
    if (_diagTick >= 20) {
      _diagTick = 0;
      _dumpRuntimeDiag('poll');
    }
  }

  int _diagTick = 0;

  /// 把当前会话诊断 + 关键状态写 %TEMP%/translator_runtime_diag.txt。
  ///
  /// 引擎内部计数（vad_segments / asr_finals / capture_rms / native_rate 等）
  /// 来自 translator_session_diag_json；UI 侧补上声源与运行态，一份就够定位
  /// 「转写不出字」断在哪一环。
  void _dumpRuntimeDiag(String where) {
    try {
      final path = lyricsSharedPath()
          .replaceAll("translator_lyrics.json", "translator_runtime_diag.txt");
      final lines = <String>[
        "time=${DateTime.now().toIso8601String()} where=$where",
        "running=$_running paused=$_paused source=$_source",
        "sources=$_sources lines=${_lines.length} partialLen=${_partial.length}",
        "sessionError=$_sessionError",
        "diag=${_bridge.sessionDiag()}",
      ];
      File(path)
          .writeAsStringSync(lines.join("\n") + "\n", mode: FileMode.append);
    } catch (e) {
      debugPrint("[diag] 落盘失败（忽略）: $e");
    }
  }

  /// 导出 SRT。
  /// 导出 SRT（字幕格式，给播放器/剪辑软件用）
  Future<String> exportSrt() => _bridge.exportSrt(_lines);

  /// 导出 TXT 对话记录（给人读的）。
  ///
  /// **从转写盘全量读** —— 运行中也能导出完整历史，
  /// 含已被内存窗口丢弃的行。
  Future<String> exportTxt() => Future.value(_bridge.sessionExport(2));

  /// 无人值守自检（TRANSLATOR_SELFTEST=1 时由 HomeShell 触发）。
  ///
  /// 完整跑一遍真实链路并写报告到 %TEMP%/translator_selftest.txt：
  ///   bootstrap → 采集能力 → start → 观察真机采集是否进音频（capture_chunks/rms）
  ///   → 开字幕窗（验证第二实例真建了窗口：proof 文件）→ 停止 → 退出。
  /// 每个判据明确标注 PASS/FAIL，跑完 exit(0)，方便连测多轮。
  Future<void> runSelfTest() async {
    final path = lyricsSharedPath()
        .replaceAll('translator_lyrics.json', 'translator_selftest.txt');
    final out = File(path);
    final log = <String>[];
    void say(String line) {
      log.add(line);
      debugPrint('[selftest] $line');
      try {
        out.writeAsStringSync(log.join('\n') + '\n');
      } catch (_) {}
    }

    say('=== selftest v1 ${DateTime.now().toIso8601String()} ===');
    await Future.delayed(const Duration(seconds: 2)); // 等 bootstrap
    say('bootstrapped=$bootstrapped error="$_sessionError"');
    say('capability=${_capability.describe()}');
    say('selfcheck=${_steps.map((e) => '${e.name}:${e.ok}').join(', ')}');
    final mp = _bridge.discoverModels();
    say('modelsFound=${mp.found} asrDir=${mp.asr}');

    // ── 采集观察 ──
    say('-- start --');
    await start();
    say('running=$running');
    int? lastChunks;
    String diagLast = '';
    for (var k = 0; k < 8; k++) {
      await Future.delayed(const Duration(seconds: 1));
      diagLast = _bridge.sessionDiag();
      final m = RegExp('"capture_chunks":([0-9]+)').firstMatch(diagLast);
      lastChunks = m != null ? int.parse(m.group(1)!) : null;
      final rms =
          RegExp('"capture_rms":([0-9.]+)').firstMatch(diagLast)?.group(1);
      final mx =
          RegExp('"capture_max_rms":([0-9.]+)').firstMatch(diagLast)?.group(1);
      if ((lastChunks ?? 0) > 0) break;
      say('  poll#$k chunks=$lastChunks rms=$rms max=$mx');
    }
    final chunks = lastChunks ?? 0;
    say('diag=${diagLast.replaceAll('\n', '')}');
    say(chunks > 0
        ? 'PASS 采集线程在回调（chunks=$chunks>0）'
        : 'FAIL 采集零回调：内放与麦克风都没拿到音频流（设备/权限问题）');

    // ── 字幕窗第二实例 ──
    say('-- lyrics window --');
    await setLyricsWindowEnabled(true);
    await Future.delayed(const Duration(seconds: 5));
    final proof = File(lyricsSharedPath()
        .replaceAll('translator_lyrics.json', 'translator_lyrics_proof.txt'));
    final shared = File(lyricsSharedPath());
    say('字幕窗 proof 文件存在=${proof.existsSync()}（第二实例真建了窗口）');
    say('共享字幕文件存在=${shared.existsSync()}');
    final procOk = _lyricsProcess != null;
    say(procOk ? 'PASS 字幕窗进程已启动 PID=${_lyricsProcess!.pid}' : 'FAIL 字幕窗进程未启动');

    // ── 停止 ──
    say('-- stop --');
    await stop();
    say('running=$running lines=${_lines.length}');
    final lines = _lines.length;
    // 安静环境 lines=0 不算失败（没人说话）——采集正常即引擎可工作（离线已实证）
    say(lines > 0
        ? 'INFO 真机出了 $lines 行字幕（含译文回填）'
        : 'INFO 未出字幕行（无人说话/无系统声音时属正常）');
    setLyricsWindowEnabled(false);
    await Future.delayed(const Duration(milliseconds: 800));
    say('=== 完成 ===');
    // ⚠️ 退出方式：SIGKILL 强杀，不用 exit(0) 也不用 sessionFree。
    // - exit(0) 会跑 Rust 静态析构 → 曾触发 0xC0000005（退出码 3221225477）
    // - sessionFree 要等停止 worker（上限 15s），卡死时拖住进程不退出
    // - SIGKILL 不过 atexit、不跑静态析构、强杀所有线程，立即干净退出
    Process.killPid(pid, ProcessSignal.sigkill);
  }

  /// 退出前杀掉独立歌词窗子进程。
  ///
  /// 进程被 TerminateProcess 强杀时**不会**触发 dispose，
  /// 不手动 kill 的话字幕窗会残留成孤儿窗口。
  void killLyricsWindow() {
    try {
      _lyricsProcess?.kill();
    } catch (_) {}
    _lyricsProcess = null;
  }

  /// 底层会话句柄地址（退出时给后台 isolate 做保存用）。0 = 会话未创建。
  int get sessionAddress => _bridge.sessionAddress;

  @override
  void dispose() {
    _pollTimer?.cancel();
    // 清理独立歌词窗：停心跳 + 杀第二实例 + 清共享文件
    _lyricsHeartbeat?.cancel();
    _lyricsHeartbeat = null;
    try {
      _lyricsProcess?.kill();
    } catch (_) {}
    _lyricsProcess = null;
    _bridge.dispose();
    super.dispose();
  }
}
