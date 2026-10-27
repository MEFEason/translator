import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

import '../state/translator_state.dart';

/// 采集能力种类（对应 Rust 侧 `CaptureCapability`）
enum CaptureKind { systemAudio, microphone, unavailable }

/// 能力探测结果
class ProbeResult {
  final CaptureKind kind;
  final String? reason;
  const ProbeResult(this.kind, this.reason);
}

/// 单个进程的负载（对应 Rust 侧 `ProcessLoad`）
class ProcessLoad {
  final String name;
  final double cpu;
  final int memBytes;
  const ProcessLoad(this.name, this.cpu, this.memBytes);
}

/// 系统资源占用快照（对应 Rust 侧 `SystemLoad`）
///
/// 判定依据是**实际 CPU / 内存占用率**，软件类别仅作微调信号。
class SystemLoad {
  /// 全局 CPU 使用率（0-100）
  final double cpu;

  /// 已用内存占比（0-100）
  final double mem;

  /// 物理内存总量（字节）
  final int memTotal;

  /// 可用物理内存（字节）
  final int memAvailable;

  /// 推荐的资源档位键：'light' | 'balanced' | 'full'
  final String recommended;

  /// 面向用户的建议文案
  final String advice;

  /// CPU 占用最高的进程
  final List<ProcessLoad> top;

  /// 检测到的实时通信类软件
  final List<String> latencySensitive;

  /// 检测到的音频播放类软件
  final List<String> audioPlayback;

  /// 推荐线程数（空闲时）。0 = 未提供（旧 DLL）。
  final int recThreadsIdle;

  /// 推荐线程数（转写中）。0 = 未提供（旧 DLL）。
  final int recThreadsRunning;

  const SystemLoad({
    required this.cpu,
    required this.mem,
    required this.memTotal,
    required this.memAvailable,
    required this.recommended,
    required this.advice,
    required this.top,
    required this.latencySensitive,
    required this.audioPlayback,
    this.recThreadsIdle = 0,
    this.recThreadsRunning = 0,
  });

  static SystemLoad fromJson(Map<String, dynamic> j) {
    List<ProcessLoad> tops = [];
    final rawTop = j['top'];
    if (rawTop is List) {
      for (final item in rawTop) {
        if (item is Map) {
          tops.add(ProcessLoad(
            (item['name'] ?? '?').toString(),
            (item['cpu'] as num?)?.toDouble() ?? 0,
            (item['mem'] as num?)?.toInt() ?? 0,
          ));
        }
      }
    }
    List<String> lat = [];
    final rawLat = j['latency_sensitive'];
    if (rawLat is List) {
      lat = rawLat.map((e) => e.toString()).toList();
    }
    List<String> aud = [];
    final rawAud = j['audio_playback'];
    if (rawAud is List) {
      aud = rawAud.map((e) => e.toString()).toList();
    }
    return SystemLoad(
      cpu: (j['cpu'] as num?)?.toDouble() ?? 0,
      mem: (j['mem'] as num?)?.toDouble() ?? 0,
      memTotal: (j['mem_total'] as num?)?.toInt() ?? 0,
      memAvailable: (j['mem_available'] as num?)?.toInt() ?? 0,
      recommended: (j['recommended'] ?? 'balanced').toString(),
      advice: (j['advice'] ?? '').toString(),
      top: tops,
      latencySensitive: lat,
      audioPlayback: aud,
      recThreadsIdle: (j['rec_threads_idle'] as num?)?.toInt() ?? 0,
      recThreadsRunning: (j['rec_threads_running'] as num?)?.toInt() ?? 0,
    );
  }
}

/// 模型目录布局（对应 Rust 侧 `ModelLayout`）
class ModelPaths {
  final String asr;
  final String mt;
  final String root;
  const ModelPaths(this.asr, this.mt, this.root);

  bool get found => asr.isNotEmpty && mt.isNotEmpty;
}

/// 资源档位（对应 Rust 侧 `ResourceTier`）
class ResourceTierInfo {
  /// 档位键：'light' | 'balanced' | 'full'
  final String key;

  /// 展示名
  final String name;

  /// 适用场景说明
  final String hint;

  /// 该档位占用的 ASR 线程数
  final int asrThreads;

  /// 该档位占用的 MT 线程数
  final int mtThreads;

  const ResourceTierInfo(
      this.key, this.name, this.hint, this.asrThreads, this.mtThreads);

  static ResourceTierInfo fromJson(Map<String, dynamic> j) {
    return ResourceTierInfo(
      (j['key'] ?? 'balanced').toString(),
      (j['name'] ?? '').toString(),
      (j['hint'] ?? '').toString(),
      (j['asr_threads'] as num?)?.toInt() ?? 2,
      (j['mt_threads'] as num?)?.toInt() ?? 2,
    );
  }
}

/// 字幕增量事件（对应 Rust 侧 `LineEvent`）
///
/// 三种 kind：
/// - `add`  一行 EN 就位（译文还没有，UI 应先上屏英文）
/// - `fill` 译文回填（同一 seq 的行补上 target）
/// - `rev`  标记待复核
class SubtitleEvent {
  final String kind;
  final int seq;
  final String? source;
  final String? target;
  final int startMs;
  final int endMs;
  final bool forcedCut;
  final bool needsReview;

  /// 说话人标签（多声源时区分「我」与「对方」）
  final String? speaker;

  const SubtitleEvent({
    required this.kind,
    required this.seq,
    this.source,
    this.target,
    this.startMs = 0,
    this.endMs = 0,
    this.forcedCut = false,
    this.needsReview = false,
    this.speaker,
  });

  factory SubtitleEvent.fromJson(Map<String, dynamic> j) {
    return SubtitleEvent(
      kind: j['k'] as String,
      seq: (j['i'] as num).toInt(),
      source: j['src'] as String?,
      target: j['tgt'] as String?,
      startMs: (j['st'] as num?)?.toInt() ?? 0,
      endMs: (j['et'] as num?)?.toInt() ?? 0,
      forcedCut: (j['cut'] as bool?) ?? false,
      needsReview: (j['rev'] as bool?) ?? false,
      speaker: j['spk'] as String?,
    );
  }
}

/// 会话状态（对应 Rust 侧 `pipeline::SessionState`）
enum SessionState { created, running, stopped, error }

/// 会话状态快照（对应 Rust 侧 `FfiSessionStatus`）
class SessionStatus {
  final SessionState state;
  final bool asrReady;
  final bool mtReady;
  final int errorCode;
  final int lastSeq;
  final int lineCount;
  final int mtDropped;
  final int transcriptWritten;
  final int transcriptFailures;

  const SessionStatus({
    required this.state,
    this.asrReady = false,
    this.mtReady = false,
    this.errorCode = 0,
    this.lastSeq = 0,
    this.lineCount = 0,
    this.mtDropped = 0,
    this.transcriptWritten = 0,
    this.transcriptFailures = 0,
  });

  bool get isRunning => state == SessionState.running;
  bool get hasError => state == SessionState.error;
}

/// 自检报告的一项（对应 Rust 侧 selfcheck 输出的 steps[]）
class SelfcheckStep {
  final String name;
  final bool ok;
  final String detail;

  const SelfcheckStep({
    required this.name,
    required this.ok,
    required this.detail,
  });

  factory SelfcheckStep.fromJson(Map<String, dynamic> j) => SelfcheckStep(
        name: j['name'] as String,
        ok: (j['ok'] as bool?) ?? false,
        detail: (j['detail'] as String?) ?? '',
      );
}

/// 转写盘统计（对应 Rust 侧 `FfiTranscriptStats`）
class TranscriptStats {
  final int written;
  final int failures;
  final int lastSeq;
  const TranscriptStats(this.written, this.failures, this.lastSeq);
}

// ── FFI 定长结构体（与 Rust 侧 #[repr(C)] 逐字段对应）──

final class FfiSessionStatus extends Struct {
  @Int32()
  external int state;
  @Int32()
  external int asrReady;
  @Int32()
  external int mtReady;
  @Int32()
  external int errorCode;
  @Uint64()
  external int lastSeq;
  @Uint64()
  external int lineCount;
  @Uint64()
  external int droppedFrames;
  @Uint64()
  external int mtDropped;
  @Uint64()
  external int transcriptWritten;
  @Uint64()
  external int transcriptFailures;
}

final class FfiTranscriptStats extends Struct {
  @Uint64()
  external int written;
  @Uint64()
  external int failures;
  @Uint64()
  external int lastSeq;
}

/// Rust 核心桥接。
///
/// ## 命名约定（AGENTS.md）
/// 库名 `translator_core`，产物 `translator_core.dll` / `.dylib` / `.so`。
///
/// ## 两级 API
///
/// 1. **采集级**（`create` / `startCapture` / `pollChunk` …）：
///    14 个导出，Android 侧 Kotlin 在用，**保留兼容**。
/// 2. **会话级**（`sessionCreate` / `sessionStart` / `takeEvents` …）：
///    桌面端走这条 —— 音频不经过 Dart，Dart 只轮询字幕事件。
///
/// ## 离线约束
/// 只加载本地动态库，**不做任何网络访问**。
class Bridge {
  late final DynamicLibrary _lib;
  final _lines = StreamController<SubtitleLine>.broadcast();

  Bridge() {
    _lib = _open();
    _bind();
  }

  /// 加载平台对应的动态库
  ///
  /// ⚠️ 顺序很重要：必须先显式 `open`，不能直接 `process()`。
  /// `DynamicLibrary.process()` 只能解析已被进程加载的 DLL 的符号，
  /// 而我们是把 DLL 放在 exe 同目录、并没有在启动时链接它。
  /// 错误写法只会报 `Failed to lookup function`，完全指不到真因。
  DynamicLibrary _open() {
    if (Platform.isWindows) {
      try {
        // Windows 加载器搜索顺序里 exe 所在目录在最前
        return DynamicLibrary.open('translator_core.dll');
      } catch (e) {
        try {
          return DynamicLibrary.process();
        } catch (_) {
          throw StateError(
            '无法加载 translator_core.dll（$e）。\n'
            '修复：先运行 scripts/build_windows.sh 生成该文件，'
            '并确认它与 translator.exe 位于同一目录。',
          );
        }
      }
    } else if (Platform.isMacOS || Platform.isIOS) {
      try {
        return DynamicLibrary.process();
      } catch (_) {
        return DynamicLibrary.open(
          'Frameworks/translator_core.framework/translator_core',
        );
      }
    } else if (Platform.isAndroid) {
      return DynamicLibrary.open('libtranslator_core.so');
    }
    throw UnsupportedError('不支持的平台: ${Platform.operatingSystem}');
  }

  // ── 采集级绑定（Android 兼容，勿删）───────────────────
  late final _CreateNative _create;
  late final _FreeNative _free;
  late final _ProbeNative _probe;
  late final _CapDescNative _capDesc;
  late final _StartNative _start;
  late final _StopNative _stop;
  late final _PollChunkNative _pollChunk;
  late final _PushChunkNative _pushChunk;
  late final _ChunkSamplesNative _chunkSamples;
  late final _SampleRateNative _sampleRate;
  late final _DroppedNative _dropped;
  late final _VersionNative _version;
  late final _IsOfflineNative _isOffline;

  // ── 会话级绑定（桌面端主链路）────────────────────────
  late final _SessionCreateNative _sessionCreate;
  late final _SessionFreeNative _sessionFree;
  late final _SessionStartNative _sessionStart;
  late final _SessionStopNative _sessionStop;
  late final _SessionPauseNative _sessionPause;
  late final _SessionTakeEventsNative _sessionTakeEvents;
  late final _SessionStatusNative _sessionStatus;
  late final _SessionErrorDescNative _sessionErrorDesc;
  late final _SessionPartialNative _sessionPartial;
  late final Pointer<Int8> Function(Pointer<Void>) _sessionPartialZh;
  late final _SessionDiagNative _sessionDiag;
  late final _SessionExportNative _sessionExport;
  late final _GlossaryLoadNative _glossaryLoad;
  late final _GlossaryReloadNative _glossaryReload;
  late final _GlossaryCountNative _glossaryCount;
  late final _UpdateLiveNative _updateLive;
  late final _SelfcheckNative _selfcheck;
  late final _ModelImportDirNative _modelImportDir;
  late final _TranscriptQueryNative _transcriptQuery;
  late final _TranscriptPathNative _transcriptPath;
  late final _TranscriptStatsNative _transcriptStats;
  late final _TranscriptDirNative _transcriptDir;
  late final _SessionCaptureCapabilityNative _sessionCaptureCapability;
  late final _SessionCaptureIndexNative _sessionCaptureIndex;

  late final _FreeStrNative _freeStrFn;

  late final _ProbeLoadNative _probeLoadFn;
  late final _ResourceTiersNative _resourceTiersFn;
  late final _DiscoverModelsNative _discoverModelsFn;

  /// 采集句柄（Android 路径用）
  Pointer<Void> _handle = nullptr;

  /// 会话句柄（桌面路径用）。会话**自己拥有**采集器。
  Pointer<Void> _session = nullptr;

  void _bind() {
    // 签名必须与 core/src/ffi.rs 的 #[no_mangle] 导出逐字节对应。
    _create = _lib.lookupFunction<Pointer<Void> Function(Int32, Int32),
        Pointer<Void> Function(int, int)>('translator_create_capture');
    _free = _lib.lookupFunction<Void Function(Pointer<Void>),
        void Function(Pointer<Void>)>('translator_free_capture');
    _probe = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_probe_capture');
    _capDesc = _lib.lookupFunction<Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_capability_desc');
    _start = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_start_capture');
    _stop = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_stop_capture');
    _pollChunk = _lib.lookupFunction<
        Uint64 Function(Pointer<Void>, Pointer<Float>, Uint64),
        int Function(
            Pointer<Void>, Pointer<Float>, int)>('translator_poll_chunk');
    _pushChunk = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Pointer<Float>, Uint64),
        int Function(
            Pointer<Void>, Pointer<Float>, int)>('translator_push_chunk');
    _chunkSamples = _lib.lookupFunction<Uint64 Function(), int Function()>(
        'translator_chunk_samples');
    _sampleRate = _lib.lookupFunction<Int32 Function(), int Function()>(
        'translator_sample_rate');
    _dropped = _lib.lookupFunction<Uint64 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_dropped_frames');
    _version =
        _lib.lookupFunction<Pointer<Int8> Function(), Pointer<Int8> Function()>(
            'translator_version');
    _isOffline = _lib.lookupFunction<Bool Function(), bool Function()>(
        'translator_is_offline');
    _freeStrFn = _lib.lookupFunction<Void Function(Pointer<Int8>),
        void Function(Pointer<Int8>)>('translator_free_string');

    // ── 系统负载探测（资源档位推荐）──
    _probeLoadFn =
        _lib.lookupFunction<Pointer<Int8> Function(), Pointer<Int8> Function()>(
            'translator_probe_system_load');
    _resourceTiersFn =
        _lib.lookupFunction<Pointer<Int8> Function(), Pointer<Int8> Function()>(
            'translator_resource_tiers');
    _discoverModelsFn =
        _lib.lookupFunction<Pointer<Int8> Function(), Pointer<Int8> Function()>(
            'translator_discover_models');

    // ── 会话级 ──
    _sessionCreate = _lib.lookupFunction<
        Pointer<Void> Function(Pointer<Int8>, Int32, Int32),
        Pointer<Void> Function(
            Pointer<Int8>, int, int)>('translator_session_create');
    _sessionFree = _lib.lookupFunction<Void Function(Pointer<Void>),
        void Function(Pointer<Void>)>('translator_session_free');
    _sessionStart = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_session_start');
    _sessionStop = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_session_stop');
    _sessionPause = _lib.lookupFunction<Int32 Function(Pointer<Void>, Int32),
        int Function(Pointer<Void>, int)>('translator_session_pause');
    _sessionTakeEvents = _lib.lookupFunction<
        Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(
            Pointer<Void>)>('translator_session_take_events');
    _sessionStatus = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Pointer<FfiSessionStatus>),
        int Function(Pointer<Void>,
            Pointer<FfiSessionStatus>)>('translator_session_status');
    _sessionErrorDesc = _lib.lookupFunction<
        Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_session_error_desc');
    _sessionPartial = _lib.lookupFunction<Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_session_partial');
    _sessionPartialZh = _lib.lookupFunction<
        Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_session_partial_zh');
    _sessionDiag = _lib.lookupFunction<Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_session_diag_json');
    _sessionExport = _lib.lookupFunction<
        Pointer<Int8> Function(Pointer<Void>, Int32),
        Pointer<Int8> Function(
            Pointer<Void>, int)>('translator_session_export');
    _glossaryLoad = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Pointer<Int8>),
        int Function(Pointer<Void>, Pointer<Int8>)>('translator_glossary_load');
    _glossaryReload = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_glossary_reload');
    _glossaryCount = _lib.lookupFunction<Int32 Function(Pointer<Void>),
        int Function(Pointer<Void>)>('translator_glossary_count');
    _updateLive = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Pointer<Utf8>),
        int Function(
            Pointer<Void>, Pointer<Utf8>)>('translator_session_update_live');
    _selfcheck = _lib.lookupFunction<Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_selfcheck_json');
    _modelImportDir = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Pointer<Int8>),
        int Function(
            Pointer<Void>, Pointer<Int8>)>('translator_model_import_dir');
    _transcriptQuery = _lib.lookupFunction<
        Pointer<Int8> Function(Pointer<Void>, Uint64, Uint32),
        Pointer<Int8> Function(
            Pointer<Void>, int, int)>('translator_transcript_query');
    _transcriptPath = _lib.lookupFunction<Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(Pointer<Void>)>('translator_transcript_path');
    _transcriptStats = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Pointer<FfiTranscriptStats>),
        int Function(Pointer<Void>,
            Pointer<FfiTranscriptStats>)>('translator_transcript_stats');
    _transcriptDir =
        _lib.lookupFunction<Pointer<Int8> Function(), Pointer<Int8> Function()>(
            'translator_transcript_dir');
    _sessionCaptureCapability = _lib.lookupFunction<
        Pointer<Int8> Function(Pointer<Void>),
        Pointer<Int8> Function(
            Pointer<Void>)>('translator_session_capture_capability');
    _sessionCaptureIndex = _lib.lookupFunction<
        Int32 Function(Pointer<Void>, Int32),
        int Function(Pointer<Void>, int)>('translator_session_capture_index');
  }

  void _freeStr(Pointer<Int8> p) => _freeStrFn(p);

  /// 读 C 字符串并释放（Rust 侧分配）
  String _takeString(Pointer<Int8> p) {
    if (p == nullptr) return '';
    final s = p.cast<Utf8>().toDartString();
    _freeStr(p);
    return s;
  }

  // ══ 采集级 API（Android 兼容）══════════════════════

  /// 探测系统资源占用（CPU / 内存），用于推荐资源档位。
  ///
  /// 底层会阻塞约 250 ms（需两次采样求 CPU 差值），
  /// 因此调用方应放到异步上下文里，别在 build 方法里同步调。
  SystemLoad probeSystemLoad() {
    final p = _probeLoadFn();
    if (p == nullptr) {
      return const SystemLoad(
        cpu: 0,
        mem: 0,
        memTotal: 0,
        memAvailable: 0,
        recommended: 'balanced',
        advice: '',
        top: [],
        latencySensitive: [],
        audioPlayback: [],
      );
    }
    final raw = _takeString(p);
    try {
      final j = jsonDecode(raw);
      if (j is Map<String, dynamic>) {
        return SystemLoad.fromJson(j);
      }
    } catch (_) {
      // 解析失败 → 用保守默认值
    }
    return const SystemLoad(
      cpu: 0,
      mem: 0,
      memTotal: 0,
      memAvailable: 0,
      recommended: 'balanced',
      advice: '',
      top: [],
      latencySensitive: [],
      audioPlayback: [],
    );
  }

  /// 自动发现模型目录（ASR / MT）。
  ///
  /// 安装后模型在 `{app}\models\`，开发态在 `translator_core\models\`。
  ModelPaths discoverModels() {
    final p = _discoverModelsFn();
    if (p == nullptr) return const ModelPaths('', '', '');
    final raw = _takeString(p);
    try {
      final j = jsonDecode(raw);
      if (j is Map<String, dynamic>) {
        return ModelPaths(
          (j['asr'] ?? '').toString(),
          (j['mt'] ?? '').toString(),
          (j['root'] ?? '').toString(),
        );
      }
    } catch (_) {}
    return const ModelPaths('', '', '');
  }

  /// 资源档位的静态说明（UI 渲染选项卡）。
  List<ResourceTierInfo> resourceTiers() {
    final p = _resourceTiersFn();
    if (p == nullptr) return const [];
    final raw = _takeString(p);
    try {
      final j = jsonDecode(raw);
      if (j is List) {
        return j
            .whereType<Map<String, dynamic>>()
            .map(ResourceTierInfo.fromJson)
            .toList();
      }
    } catch (_) {}
    return const [];
  }

  /// 创建采集句柄。source: 0=系统音频 1=麦克风；runtime: 0=原生 1=卓易通
  bool create(int source, int runtime) {
    _handle = _create(source, runtime);
    return _handle != nullptr;
  }

  int get chunkSamples => _chunkSamples();

  int get sampleRate => _sampleRate();

  bool get isOffline => _isOffline();

  String version() {
    final p = _version();
    if (p == nullptr) return 'unknown';
    return _takeString(p);
  }

  /// 探测采集能力。
  ///
  /// ## 走 session 而不是独立采集句柄
  ///
  /// 桌面端的采集器由**会话自己拥有**（`FfiSession.capture`），
  /// 独立句柄那条路径在桌面端既没被创建过，也拿不到真实状态 ——
  /// 早期版本因此在设置页误报「采集不可用：采集句柄未创建」，
  /// 而实际上内放采集一直是好的。
  ///
  /// 所以：优先读会话采集器的真实能力；会话不存在时（Android 早期路径）
  /// 才退回独立句柄。
  Future<ProbeResult> probeCapture() async {
    if (_session != nullptr) {
      final raw = _takeString(_sessionCaptureCapability(_session));
      if (raw.isNotEmpty) {
        try {
          final j = jsonDecode(raw) as Map<String, dynamic>;
          final kind = switch (j['kind']) {
            'systemAudio' => CaptureKind.systemAudio,
            'microphone' => CaptureKind.microphone,
            _ => CaptureKind.unavailable,
          };
          final reason = j['reason'] as String?;
          return ProbeResult(
            kind,
            (reason == null || reason.isEmpty) ? null : reason,
          );
        } catch (_) {
          // JSON 解析失败 → 继续走独立句柄兜底
        }
      }
    }
    if (_handle == nullptr) {
      return const ProbeResult(CaptureKind.unavailable, '采集句柄未创建');
    }
    final code = _probe(_handle);
    return switch (code) {
      0 => const ProbeResult(CaptureKind.systemAudio, null),
      1 => const ProbeResult(CaptureKind.microphone, '内放采集不可用'),
      _ => const ProbeResult(CaptureKind.unavailable, '无可用音频设备'),
    };
  }

  /// 会话采集能力 JSON（原始），供调试与状态页展示。
  String sessionCaptureCapabilityJson() {
    if (_session == nullptr) return '';
    return _takeString(_sessionCaptureCapability(_session));
  }

  /// 查某类声源在会话采集器列表中的下标（Android 注入链路用）。
  ///
  /// [kind]：0 = 系统内放，1 = 麦克风。
  /// 返回 -1 = 该类声源的采集器没建起来（不可用）。
  ///
  /// 为什么不能写死 0：会话的采集器列表**只含创建成功的**，
  /// 下标与 `cfg.sources` 可能错位（如卓易通容器内内放建不起来）。
  int captureIndex(int kind) {
    if (_session == nullptr) return -1;
    return _sessionCaptureIndex(_session, kind);
  }

  String capabilityDesc() {
    if (_session != nullptr) {
      final raw = sessionCaptureCapabilityJson();
      if (raw.isNotEmpty) return raw;
    }
    if (_handle == nullptr) return '采集句柄未创建';
    return _takeString(_capDesc(_handle));
  }

  Future<void> startCapture() async {
    if (_handle == nullptr) throw StateError('采集句柄未创建');
    _start(_handle);
  }

  Future<void> stopCapture() async {
    if (_handle == nullptr) return;
    _stop(_handle);
  }

  List<double> pollChunk() {
    if (_handle == nullptr) return const [];
    final n = chunkSamples;
    final out = calloc<Float>(n);
    try {
      final got = _pollChunk(_handle, out, n);
      if (got <= 0) return const [];
      return List<double>.generate(got, (i) => out[i]);
    } finally {
      calloc.free(out);
    }
  }

  int droppedFrames() => _handle == nullptr ? 0 : _dropped(_handle);

  // ══ 会话级 API（桌面端主链路）═══════════════════════

  /// 创建会话（含采集器）。
  ///
  /// [cfgJson] 字段见 `core/src/ffi_session.rs` 的 `config_from_json`：
  /// `asr_model_dir` / `mt_model_dir` / `glossary_paths` / `profile` /
  /// `asr_threads` / `mt_threads` / `mt_accurate` /
  /// `memory_window_lines` / `backend` / `transcript_dir`。
  ///
  /// **不加载引擎**（毫秒返回），引擎在 `sessionStart` 的监督线程里加载。
  bool sessionCreate(String cfgJson, int source, int runtime) {
    final p = cfgJson.toNativeUtf8();
    try {
      _session = _sessionCreate(p.cast(), source, runtime);
      return _session != nullptr;
    } finally {
      calloc.free(p);
    }
  }

  /// 启动会话。返回 0 = 成功。
  ///
  /// 立即返回：引擎加载在监督线程内完成，UI 轮询 [sessionStatus]。
  int sessionStart() {
    if (_session == nullptr) return 7; // InvalidArg
    return _sessionStart(_session);
  }

  /// 暂停 / 恢复转写。
  ///
  /// **暂停保留会话与音频接收端**，恢复后接着转 ——
  /// 与 `sessionStop`（真结束）不同。
  int sessionPause(bool paused) {
    if (_session == nullptr) return 7;
    return _sessionPause(_session, paused ? 1 : 0);
  }

  /// 停止会话（同步冲刷最后一句的译文）。
  int sessionStop() {
    if (_session == nullptr) return 7;
    return _sessionStop(_session);
  }

  /// 会话是否已创建（未创建则 start 无意义）
  bool get sessionStarted => _session != nullptr;

  /// 会话句柄地址（给后台 isolate 用）。
  ///
  /// `DynamicLibrary` 不能跨 isolate 传递，但**同一个进程内**重新
  /// `open('translator_core.dll')` 拿到的还是同一份加载实例，
  /// 于是后台 isolate 可以用 `Pointer.fromAddress(addr)` 还原句柄调 FFI。
  /// 返回 0 表示会话未创建。
  int get sessionAddress => _session.address;

  void sessionFree() {
    if (_session == nullptr) return;
    _sessionFree(_session);
    _session = nullptr;
  }

  /// 取走自上次调用以来的增量字幕事件。
  List<SubtitleEvent> takeEvents() {
    if (_session == nullptr) return const [];
    final p = _sessionTakeEvents(_session);
    if (p == nullptr) return const [];
    final raw = _takeString(p);
    if (raw.isEmpty) return const [];
    try {
      final j = jsonDecode(raw) as Map<String, dynamic>;
      final list = (j['events'] as List?) ?? const [];
      return list
          .cast<Map<String, dynamic>>()
          .map(SubtitleEvent.fromJson)
          .toList();
    } catch (_) {
      return const [];
    }
  }

  SessionStatus sessionStatus() {
    if (_session == nullptr) {
      return const SessionStatus(state: SessionState.created);
    }
    final out = calloc<FfiSessionStatus>();
    try {
      _sessionStatus(_session, out);
      return SessionStatus(
        state: switch (out.ref.state) {
          1 => SessionState.running,
          2 => SessionState.stopped,
          3 => SessionState.error,
          _ => SessionState.created,
        },
        asrReady: out.ref.asrReady != 0,
        mtReady: out.ref.mtReady != 0,
        errorCode: out.ref.errorCode,
        lastSeq: out.ref.lastSeq,
        lineCount: out.ref.lineCount,
        mtDropped: out.ref.mtDropped,
        transcriptWritten: out.ref.transcriptWritten,
        transcriptFailures: out.ref.transcriptFailures,
      );
    } finally {
      calloc.free(out);
    }
  }

  /// 错误详情（如「模型未安装」「词库第 12 行解析失败」）
  String sessionErrorDesc() {
    if (_session == nullptr) return '';
    return _takeString(_sessionErrorDesc(_session));
  }

  /// partial 预览文本
  String sessionPartial() {
    if (_session == nullptr) return '';
    return _takeString(_sessionPartial(_session));
  }

  /// 流式译文（英文预览的实时中文，与 sessionPartial 配对，同帧读取）。
  String sessionPartialZh() {
    if (_session == nullptr) return '';
    return _takeString(_sessionPartialZh(_session));
  }

  /// 引擎诊断 JSON（vad_segments / asr_finals / capture_rms 等）。
  ///
  /// 运行时排查「转写不出字」的关键：靠它区分问题在
  /// 采集（capture_rms 低）、VAD（vad_segments=0）还是 ASR（asr_finals=0）。
  String sessionDiag() {
    if (_session == nullptr) return '';
    return _takeString(_sessionDiag(_session));
  }

  /// 导出字幕。format: 0=SRT 1=VTT 2=TXT
  String sessionExport(int format) {
    if (_session == nullptr) return '';
    return _takeString(_sessionExport(_session, format));
  }

  /// 导入词库文件，返回词条数（负数 = 错误码）
  int glossaryLoad(String path) {
    if (_session == nullptr) return 7;
    final p = path.toNativeUtf8();
    try {
      return _glossaryLoad(_session, p.cast());
    } finally {
      calloc.free(p);
    }
  }

  int glossaryReload() {
    if (_session == nullptr) return 7;
    return _glossaryReload(_session);
  }

  /// 当前生效词条总数（负数 = 错误码，会话未创建返回 7）。
  int glossaryCount() {
    if (_session == nullptr) return 7;
    return _glossaryCount(_session);
  }

  /// 实时更新转写参数（2026-10-25：转写中动态调整，无需重启会话）。
  ///
  /// [cfgJson] 形如 `{"asr_threads":8,"hangover_ms":600,"offline_refine":true}`，
  /// 缺省项不变。
  ///
  /// 生效时机：
  /// - `hangover_ms` / `offline_refine`：下一句立即生效
  /// - `asr_threads`：下一句边界热切换 recognizer（会短暂丢左上下文）
  ///
  /// 返回 0 = 已应用（`FfiError::Ok`）。
  int updateLiveConfig(String cfgJson) {
    if (_session == nullptr) return 7;
    final p = cfgJson.toNativeUtf8();
    try {
      return _updateLive(_session, p.cast());
    } finally {
      calloc.free(p);
    }
  }

  /// 启动自检（5 步）
  List<SelfcheckStep> selfcheck() {
    if (_session == nullptr) return const [];
    final raw = _takeString(_selfcheck(_session));
    if (raw.isEmpty) return const [];
    try {
      final j = jsonDecode(raw) as Map<String, dynamic>;
      return ((j['steps'] as List?) ?? const [])
          .cast<Map<String, dynamic>>()
          .map(SelfcheckStep.fromJson)
          .toList();
    } catch (_) {
      return const [];
    }
  }

  /// 导入模型目录。返回 0 成功，非 0 为错误码。
  ///
  /// 阶段一只做**校验与登记**（目录里有没有可识别的模型），
  /// 不做复制；`.dshm` 压缩包解压留到阶段三。
  int modelImportDir(String dir) {
    if (_session == nullptr) return 7;
    final p = dir.toNativeUtf8();
    try {
      return _modelImportDir(_session, p.cast());
    } finally {
      calloc.free(p);
    }
  }

  /// 从转写盘按 seq 范围取行（向上滚动回看历史）
  List<SubtitleLine> transcriptQuery(int fromSeq, int maxLines) {
    if (_session == nullptr) return const [];
    final raw = _takeString(_transcriptQuery(_session, fromSeq, maxLines));
    if (raw.isEmpty) return const [];
    try {
      final j = jsonDecode(raw) as Map<String, dynamic>;
      return ((j['lines'] as List?) ?? const [])
          .cast<Map<String, dynamic>>()
          .map((l) => SubtitleLine(
                seq: (l['seq'] as num?)?.toInt() ?? 0,
                source: (l['src'] as String?) ?? '',
                target: l['tgt'] as String?,
                startMs: (l['st'] as num?)?.toInt() ?? 0,
                endMs: (l['et'] as num?)?.toInt() ?? 0,
                needsReview: (l['rev'] as bool?) ?? false,
                forcedCut: (l['cut'] as bool?) ?? false,
              ))
          .toList();
    } catch (_) {
      return const [];
    }
  }

  /// 本场转写盘文件路径
  String transcriptPath() {
    if (_session == nullptr) return '';
    return _takeString(_transcriptPath(_session));
  }

  TranscriptStats transcriptStats() {
    if (_session == nullptr) return const TranscriptStats(0, 0, 0);
    final out = calloc<FfiTranscriptStats>();
    try {
      _transcriptStats(_session, out);
      return TranscriptStats(
          out.ref.written, out.ref.failures, out.ref.lastSeq);
    } finally {
      calloc.free(out);
    }
  }

  /// 默认转写盘目录（桌面 translate results）
  String transcriptDir() {
    final p = _transcriptDir();
    if (p == nullptr) return '';
    return _takeString(p);
  }

  // ══ 字幕流 ════════════════════════════════════════

  Stream<SubtitleLine> get subtitleStream => _lines.stream;

  /// 推入一行字幕（供 [TranslatorState] 的轮询驱动调用）
  void emitLine(SubtitleLine line) => _lines.add(line);

  /// Dart 侧本地导出（不调 Rust —— Rust 侧也有导出，
  /// 但 Dart 侧拼接更快且不依赖会话存活）
  Future<String> exportSrt(List<SubtitleLine> lines) async {
    final buf = StringBuffer();
    for (var i = 0; i < lines.length; i++) {
      final l = lines[i];
      buf.writeln('${i + 1}');
      buf.writeln('${l.srtStamp} --> ${_stamp(l.endMs)}');
      buf.writeln(l.source);
      if (l.target != null) buf.writeln(l.target);
      buf.writeln();
    }
    return buf.toString();
  }

  String _stamp(int ms) {
    final h = ms ~/ 3600000;
    final m = (ms % 3600000) ~/ 60000;
    final s = (ms % 60000) ~/ 1000;
    final mss = ms % 1000;
    return '${h.toString().padLeft(2, '0')}:'
        '${m.toString().padLeft(2, '0')}:'
        '${s.toString().padLeft(2, '0')},'
        '${mss.toString().padLeft(3, '0')}';
  }

  void dispose() {
    sessionFree();
    if (_handle != nullptr) {
      _free(_handle);
      _handle = nullptr;
    }
    _lines.close();
  }
}

// ── 原生函数类型（与 core/src/ffi*.rs 严格对应）───────

typedef _CreateNative = Pointer<Void> Function(int, int);
typedef _FreeNative = void Function(Pointer<Void>);
typedef _ProbeNative = int Function(Pointer<Void>);
typedef _CapDescNative = Pointer<Int8> Function(Pointer<Void>);
typedef _StartNative = int Function(Pointer<Void>);
typedef _StopNative = int Function(Pointer<Void>);
typedef _PollChunkNative = int Function(Pointer<Void>, Pointer<Float>, int);
typedef _PushChunkNative = int Function(Pointer<Void>, Pointer<Float>, int);
typedef _ChunkSamplesNative = int Function();
typedef _SampleRateNative = int Function();
typedef _DroppedNative = int Function(Pointer<Void>);
typedef _VersionNative = Pointer<Int8> Function();
typedef _IsOfflineNative = bool Function();
typedef _FreeStrNative = void Function(Pointer<Int8>);
typedef _ProbeLoadNative = Pointer<Int8> Function();
typedef _ResourceTiersNative = Pointer<Int8> Function();
typedef _DiscoverModelsNative = Pointer<Int8> Function();

typedef _SessionCreateNative = Pointer<Void> Function(Pointer<Int8>, int, int);
typedef _SessionFreeNative = void Function(Pointer<Void>);
typedef _SessionStartNative = int Function(Pointer<Void>);
typedef _SessionStopNative = int Function(Pointer<Void>);
typedef _SessionPauseNative = int Function(Pointer<Void>, int);
typedef _SessionTakeEventsNative = Pointer<Int8> Function(Pointer<Void>);
typedef _SessionStatusNative = int Function(
    Pointer<Void>, Pointer<FfiSessionStatus>);
typedef _SessionErrorDescNative = Pointer<Int8> Function(Pointer<Void>);
typedef _SessionPartialNative = Pointer<Int8> Function(Pointer<Void>);
typedef _SessionDiagNative = Pointer<Int8> Function(Pointer<Void>);
typedef _SessionExportNative = Pointer<Int8> Function(Pointer<Void>, int);
typedef _GlossaryLoadNative = int Function(Pointer<Void>, Pointer<Int8>);
typedef _GlossaryReloadNative = int Function(Pointer<Void>);
typedef _GlossaryCountNative = int Function(Pointer<Void>);
typedef _UpdateLiveNative = int Function(Pointer<Void>, Pointer<Utf8>);
typedef _SelfcheckNative = Pointer<Int8> Function(Pointer<Void>);
typedef _ModelImportDirNative = int Function(Pointer<Void>, Pointer<Int8>);
typedef _TranscriptQueryNative = Pointer<Int8> Function(
    Pointer<Void>, int, int);
typedef _TranscriptPathNative = Pointer<Int8> Function(Pointer<Void>);
typedef _TranscriptStatsNative = int Function(
    Pointer<Void>, Pointer<FfiTranscriptStats>);
typedef _TranscriptDirNative = Pointer<Int8> Function();
typedef _SessionCaptureCapabilityNative = Pointer<Int8> Function(Pointer<Void>);
typedef _SessionCaptureIndexNative = int Function(Pointer<Void>, int);
