import 'dart:convert';
import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/translator_state.dart';
import '../services/responsive.dart';
import '../services/tray_service.dart' show startDragFloatingWindow;

/// 字幕主界面。
///
/// ## 核心设计：EN 先于 ZH
///
/// 翻译慢于识别时，英文立即上屏，中文位置显示占位符。
/// 这是 MT 背压策略（DESIGN §6.2）——
/// **绝不阻塞转写显示**，否则用户会以为程序卡死。
///
/// ## partial 不入正式字幕
///
/// 流式识别器在句子中途会大幅改写前文，直接显示会剧烈跳动。
/// partial 只在底部以灰色预览态显示，只有 VAD 断句后的 final
/// 才成为正式字幕行（DESIGN §6.2）。
class SubtitlePage extends StatelessWidget {
  const SubtitlePage({super.key});

  @override
  Widget build(BuildContext context) {
    final state = context.watch<TranslatorState>();

    // ── 悬浮字幕模式 ──
    // 置顶 + 无边框的小窗，只放字幕。特征（用户明确要求）：
    //   - **底部半透明**：整个窗体不透明，但字幕列表区用半透明深色底，
    //     既盖得住下面的内容，又不完全挡住
    //   - **可拖动**：见 [_FloatingDragHandle]
    //   - **可调字号**：设置页滑块 → state.subtitleFontSize
    if (state.floatingMode) {
      return _FloatingSubtitleView(state: state);
    }

    return Scaffold(
      appBar: AppBar(
        title: const Text('字幕'),
        actions: [
          IconButton(
            tooltip: '导出 TXT 对话记录',
            onPressed:
                state.lines.isEmpty ? null : () => _export(context, state),
            icon: const Icon(Icons.save_alt),
          ),
          IconButton(
            tooltip: '结束会话',
            onPressed:
                state.running ? () => _confirmStop(context, state) : null,
            icon: const Icon(Icons.stop_circle_outlined),
          ),
        ],
      ),
      floatingActionButton: FloatingActionButton.extended(
        // 三态：未开始 → 开始；运行中 → 暂停；暂停中 → 继续
        //
        // 「暂停」与「结束」是两件事：暂停可恢复（同一会话、seq 连续、
        // 同一个转写盘文件），结束是真收工。混成一个按钮时，
        // 用户点了「暂停」就再也接不上 —— 那是最初的 bug。
        onPressed: () {
          if (!state.running) {
            state.start();
          } else if (state.paused) {
            state.resume();
          } else {
            state.pause();
          }
        },
        icon: Icon(
          !state.running
              ? Icons.play_arrow
              : (state.paused ? Icons.play_arrow : Icons.pause),
        ),
        label: Text(
          !state.running ? '开始转写' : (state.paused ? '继续' : '暂停'),
        ),
      ),
      body: Column(
        children: [
          _StatusBar(state: state),
          // 翻译引擎未启用时**明示**（阶段1：只装 ASR 模型）。
          // 禁止静默降级 —— 用户必须知道为什么没有译文（DESIGN §2.5.2）。
          if (!state.mtAvailable) const _MtDisabledNotice(),
          if (state.transcriptPath.isNotEmpty) _TranscriptBar(state: state),
          Expanded(
            // ★ 流式当前行（用户 2026-10-24 定案）：识别中的文本要**长在字幕
            // 列表里**——先占位符，逐字出来，说完才定型成锁定行。不能只在
            // 底部挂预览条（用户实测：预览有字、列表却是空的，像丢字幕）。
            child: (state.lines.isEmpty &&
                    state.history.isEmpty &&
                    state.partial.isEmpty &&
                    state.partialZh.isEmpty)
                ? _EmptyHint(running: state.running)
                : ResponsiveBody(
                    // 平板横屏时字幕行限宽居中：一行字横跨 1280dp 没法读，
                    // 视线要来回扫。限宽后阅读节奏稳定。
                    child: ListView.builder(
                      reverse: true, // 最新在底部，贴近字幕习惯
                      // +1 当前行（视觉底部） +1 历史加载器（视觉顶部）
                      itemCount: state.lines.length + state.history.length + 2,
                      itemBuilder: (ctx, i) {
                        // reverse 列表的 i=0 = 视觉最底 = 流式当前行
                        if (i == 0) {
                          return _StreamingLine(
                            partial: state.partial,
                            partialZh: state.partialZh,
                            running: state.running,
                          );
                        }
                        final j = i - 1;
                        // 最旧的在最上面。顺序：history（旧→新） + lines（旧→新）
                        final total = state.lines.length + state.history.length;
                        if (j == total) {
                          // 这是 reverse 列表的「顶部」项
                          return _HistoryFooter(state: state);
                        }
                        final idx = total - 1 - j;
                        final line = idx < state.history.length
                            ? state.history[idx]
                            : state.lines[idx - state.history.length];
                        return SubtitleRow(line: line);
                      },
                    ),
                  ),
          ),
        ],
      ),
    );
  }

  /// 结束会话（真收工）。与「暂停」不同：结束后再开始是**新会话**。
  Future<void> _confirmStop(BuildContext context, TranslatorState state) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('结束会话？'),
        content: const Text(
          '会冲刷最后一句的译文并保存转写盘。\n'
          '结束后若要再转写，会开启一场新会话（字幕从新的一段开始）。',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(ctx, false),
            child: const Text('取消'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(ctx, true),
            child: const Text('结束'),
          ),
        ],
      ),
    );
    if (ok == true) {
      await state.stop();
    }
  }

  /// 导出为 `.txt` 对话记录并**真正写入磁盘**。
  ///
  /// 之前这里只生成字符串就弹个 Toast（"待写入文件"）——
  /// 半个功能。用户要的导出就是拿到文件。
  Future<void> _export(BuildContext context, TranslatorState state) async {
    final text = await state.exportTxt();
    if (text.isEmpty) {
      if (!context.mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('还没有可导出的字幕')),
      );
      return;
    }

    // 让用户选保存位置；取消则回退到转写盘目录
    final name =
        'transcript-${DateTime.now().toIso8601String().replaceAll(RegExp(r'[:.]'), '-')}.txt';

    // ⚠️ Android：file_picker 8.x 的 saveFile **在 Android 上不支持**
    // （会抛 UnimplementedError / 返回 null）—— 走应用私有目录落盘。
    // Android 上导出到 getExternalFilesDir()，用户可通过文件管理器
    // 或 adb 取出（外部私有目录在 Android 10+ 需要 MediaStore 才能被
    // 第三方浏览，这里先保证「文件确实写出来了」）。
    String? target;
    if (Platform.isAndroid) {
      final dir = state.transcriptPath;
      if (dir.isEmpty) {
        if (!context.mounted) return;
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(content: Text('未设置转写盘目录，无法导出')),
        );
        return;
      }
      target = '$dir${Platform.pathSeparator}$name';
    } else {
      final picked = await FilePicker.platform.saveFile(
        dialogTitle: '导出字幕',
        fileName: name,
        type: FileType.custom,
        allowedExtensions: const ['txt'],
      );
      target = picked;
    }

    if (target == null) {
      // 回退：写到转写盘所在目录（用户已见过这个位置）
      final dir = state.transcriptPath;
      if (dir.isEmpty) {
        if (!context.mounted) return;
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(content: Text('已取消导出')),
        );
        return;
      }
      // transcriptPath 就是**目录**本身（2026-10-25 语义纠正：
      // 以前误以为是文件路径而取 parent，回退位置跑到桌面上一级）
      target = '$dir${Platform.pathSeparator}$name';
    }

    try {
      final f = File(target);
      await f.writeAsString(text, encoding: utf8, flush: true);
      if (!context.mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('已导出 ${text.length} 字符 → $target')),
      );
    } catch (e) {
      if (!context.mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('导出失败：$e')),
      );
    }
  }
}

/// 状态条
class _StatusBar extends StatelessWidget {
  final TranslatorState state;
  const _StatusBar({required this.state});

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      color: Theme.of(context).colorScheme.surfaceContainerHighest,
      child: Row(
        children: [
          Icon(
            state.running ? Icons.circle : Icons.stop_circle_outlined,
            size: 12,
            color: state.running ? Colors.green : Colors.grey,
          ),
          const SizedBox(width: 6),
          Text(state.running ? '转写中' : '已停止',
              style: const TextStyle(fontSize: 11)),
          const Spacer(),
          if (state.translatingCount > 0)
            Text('翻译中 ${state.translatingCount}',
                style: const TextStyle(fontSize: 11)),
          if (state.reviewCount > 0) ...[
            const SizedBox(width: 8),
            Icon(Icons.error_outline, size: 12, color: Colors.orange.shade700),
            Text('待复核 ${state.reviewCount}',
                style: TextStyle(fontSize: 11, color: Colors.orange.shade700)),
          ],
        ],
      ),
    );
  }
}

/// 翻译引擎未启用提示条。
///
/// ## 为什么必须有
///
/// 阶段1（Android 先跑 ASR）设备上只装了 ASR 模型，没有 Bergamot 权重。
/// 此时字幕**只有原文、没有译文**。若不提示，用户会以为程序坏了
/// （「说了话怎么不翻译？」）—— 这是最差体验。
///
/// DESIGN §2.5.2 明确要求：**禁止静默降级**。
class _MtDisabledNotice extends StatelessWidget {
  const _MtDisabledNotice();

  @override
  Widget build(BuildContext context) {
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      color: Colors.amber.shade100,
      child: Row(
        children: [
          const Icon(Icons.translate, size: 14, color: Colors.deepOrange),
          const SizedBox(width: 6),
          const Expanded(
            child: Text(
              '翻译引擎未启用：未安装翻译权重，当前仅显示识别原文。',
              style: TextStyle(fontSize: 11, color: Colors.deepOrange),
            ),
          ),
        ],
      ),
    );
  }
}

/// 单条字幕行
class SubtitleRow extends StatelessWidget {
  final SubtitleLine line;
  const SubtitleRow({super.key, required this.line});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      decoration: BoxDecoration(
        border: Border(
            bottom:
                BorderSide(color: theme.dividerColor.withValues(alpha: 0.2))),
        color: line.needsReview ? Colors.orange.withValues(alpha: 0.06) : null,
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              // 说话人标签：多声源时区分「我」与「对方」。
              // 内放（对方）用主色、麦克风（我）用强调色，一眼可分。
              if (line.speaker != null) ...[
                Container(
                  padding:
                      const EdgeInsets.symmetric(horizontal: 6, vertical: 1),
                  decoration: BoxDecoration(
                    color: (line.speaker == '我'
                            ? theme.colorScheme.secondaryContainer
                            : theme.colorScheme.primaryContainer)
                        .withValues(alpha: 0.6),
                    borderRadius: BorderRadius.circular(4),
                  ),
                  child: Text(
                    line.speaker!,
                    style: theme.textTheme.labelSmall?.copyWith(
                      fontWeight: FontWeight.w600,
                    ),
                  ),
                ),
                const SizedBox(width: 6),
              ],
              Text(
                '${(line.startMs / 1000).toStringAsFixed(1)}s',
                style:
                    theme.textTheme.bodySmall?.copyWith(color: theme.hintColor),
              ),
              if (line.needsReview) ...[
                const SizedBox(width: 6),
                Icon(Icons.warning_amber,
                    size: 12, color: Colors.orange.shade700),
                Text('术语待复核',
                    style:
                        TextStyle(fontSize: 10, color: Colors.orange.shade700)),
              ],
              if (line.forcedCut) ...[
                const SizedBox(width: 6),
                Icon(Icons.content_cut, size: 12, color: theme.hintColor),
              ],
            ],
          ),
          const SizedBox(height: 4),
          Text(line.source, style: theme.textTheme.bodyMedium),
          const SizedBox(height: 2),
          Text(
            // EN 先于 ZH：翻译中显示占位符而非留空
            line.target ?? '░░░ 翻译中…',
            style: theme.textTheme.bodyMedium?.copyWith(
              color: line.target == null ? theme.hintColor : null,
              fontWeight:
                  line.target == null ? FontWeight.normal : FontWeight.w500,
            ),
          ),
        ],
      ),
    );
  }
}

/// partial 预览（灰色，不入正式字幕）
class _PartialPreview extends StatelessWidget {
  final String text;
  const _PartialPreview({required this.text});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(10),
      color: theme.colorScheme.surfaceContainerHighest,
      child: Text(
        text,
        style: theme.textTheme.bodySmall?.copyWith(
          color: theme.hintColor,
          fontStyle: FontStyle.italic,
        ),
        maxLines: 2,
        overflow: TextOverflow.ellipsis,
      ),
    );
  }
}

/// 空态提示
class _EmptyHint extends StatelessWidget {
  final bool running;
  const _EmptyHint({required this.running});

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Text(
        running ? '等待语音输入…' : '点击「开始转写」',
        style: Theme.of(context).textTheme.bodyMedium?.copyWith(
              color: Theme.of(context).hintColor,
            ),
      ),
    );
  }
}

/// 流式当前行：字幕列表的「正在识别的那一行」。
///
/// - 有预览文本 → 显示文本 + 闪烁光标（逐字生长中）
/// - 还没出字 → 只显示呼吸闪烁的占位符（「正在听」可见）
/// - 未在转写 → 不显示（外层已保证只有 running 或有 partial 才进来）
class _StreamingLine extends StatelessWidget {
  final String partial;

  /// 流式译文（英文预览的实时中文，双语同帧生长）。
  final String partialZh;
  final bool running;
  const _StreamingLine(
      {required this.partial, required this.partialZh, required this.running});

  @override
  Widget build(BuildContext context) {
    final t = partial.trim();
    final z = partialZh.trim();
    final cs = Theme.of(context).colorScheme;
    final base = Theme.of(context).textTheme.bodyLarge?.fontSize ?? 16;
    if (t.isEmpty && z.isEmpty) {
      return Align(
        alignment: Alignment.centerLeft,
        child: _BlinkCursor(size: base, color: cs.primary),
      );
    }
    // 双语流式：英文 + 光标在上，实时译文在下
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (t.isNotEmpty)
          RichText(
            text: TextSpan(
              children: [
                TextSpan(
                  text: t,
                  style: Theme.of(context).textTheme.bodyLarge?.copyWith(
                        color: cs.primary,
                        fontWeight: FontWeight.w600,
                      ),
                ),
                WidgetSpan(
                  alignment: PlaceholderAlignment.middle,
                  child: _BlinkCursor(size: base, color: cs.primary),
                ),
              ],
            ),
          ),
        if (z.isNotEmpty)
          Text(
            z,
            style: Theme.of(context).textTheme.bodyMedium?.copyWith(
                  color: Colors.amber.shade700,
                ),
          ),
      ],
    );
  }
}

/// 呼吸式闪烁光标 ▍（等候占位符 + 识别光标两用）。
class _BlinkCursor extends StatefulWidget {
  final double size;
  final Color color;
  const _BlinkCursor({required this.size, required this.color});

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
      child: Text('▍',
          style: TextStyle(fontSize: widget.size, color: widget.color)),
    );
  }
}

/// 转写盘信息条：告诉用户「更早的内容在文件里」，别以为历史丢了
class _TranscriptBar extends StatelessWidget {
  final TranslatorState state;
  const _TranscriptBar({required this.state});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final st = state.transcriptStats;
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 4),
      color: theme.colorScheme.surfaceContainerHighest.withValues(alpha: 0.5),
      child: Row(
        children: [
          Icon(Icons.folder_outlined, size: 12, color: theme.hintColor),
          const SizedBox(width: 6),
          Expanded(
            child: Text(
              '转写盘 ${st.written} 条 · ${state.transcriptPath}',
              style:
                  theme.textTheme.labelSmall?.copyWith(color: theme.hintColor),
              overflow: TextOverflow.ellipsis,
            ),
          ),
          if (st.failures > 0)
            Text('写入失败 ${st.failures}',
                style: TextStyle(fontSize: 10, color: Colors.red.shade700)),
        ],
      ),
    );
  }
}

/// 列表顶部的「加载更早」指示
class _HistoryFooter extends StatelessWidget {
  final TranslatorState state;
  const _HistoryFooter({required this.state});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    if (state.loadingHistory) {
      return const Padding(
        padding: EdgeInsets.all(12),
        child: Center(
          child: SizedBox(
            width: 16,
            height: 16,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
        ),
      );
    }
    if (!state.hasMoreHistory) {
      // 已经到文件开头了
      return Padding(
        padding: const EdgeInsets.all(12),
        child: Center(
          child: Text(
            state.history.isEmpty ? '没有更早的字幕' : '—— 已到最早 ——',
            style: theme.textTheme.labelSmall?.copyWith(color: theme.hintColor),
          ),
        ),
      );
    }
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Center(
        child: TextButton.icon(
          onPressed: () => state.loadMoreHistory(),
          icon: const Icon(Icons.history, size: 14),
          label: const Text('加载更早的字幕', style: TextStyle(fontSize: 11)),
        ),
      ),
    );
  }
}

/// 悬浮字幕视图。
///
/// 置顶小窗里只放字幕：顶部一条可拖动把手，中间半透明深色字幕区，
/// 底部一行半透明控制条（开始/暂停 + 退出悬浮）。
class _FloatingSubtitleView extends StatelessWidget {
  const _FloatingSubtitleView({required this.state});

  final TranslatorState state;

  @override
  Widget build(BuildContext context) {
    return Material(
      // 窗体本身透明 —— 半透明效果由下面的字幕区自己控制，
      // 这样把手和控制条可以是不透的，字幕区是半透的。
      color: Colors.transparent,
      child: Column(
        children: [
          // 拖动把手：按住它拖动整个悬浮窗
          _FloatingDragHandle(state: state),
          // 字幕区：半透明深色底（用户要求「底部半透明」）
          Expanded(
            child: Container(
              margin: const EdgeInsets.symmetric(horizontal: 6),
              decoration: BoxDecoration(
                color: Colors.black.withValues(alpha: 0.62),
                borderRadius: BorderRadius.circular(10),
              ),
              child: state.lines.isEmpty && state.partial.isEmpty
                  ? const Center(
                      child: Text('等待语音…',
                          style:
                              TextStyle(color: Colors.white70, fontSize: 13)),
                    )
                  : ListView(
                      padding: const EdgeInsets.all(10),
                      reverse: true,
                      children: [
                        if (state.partial.isNotEmpty)
                          Padding(
                            padding: const EdgeInsets.only(bottom: 6),
                            child: Text(
                              state.partial,
                              style: TextStyle(
                                color: Colors.white70,
                                fontSize: state.subtitleFontSize * 0.8,
                              ),
                            ),
                          ),
                        // 最新在下，故倒序展示
                        for (final l in state.lines.reversed)
                          Padding(
                            padding: const EdgeInsets.only(bottom: 8),
                            child: Column(
                              crossAxisAlignment: CrossAxisAlignment.start,
                              children: [
                                Text(
                                  l.source,
                                  style: TextStyle(
                                    color: Colors.white,
                                    fontSize: state.subtitleFontSize,
                                    height: 1.3,
                                  ),
                                ),
                                if (l.target != null && l.target!.isNotEmpty)
                                  Text(
                                    l.target!,
                                    style: TextStyle(
                                      color: Colors.amber.shade200,
                                      fontSize: state.subtitleFontSize * 0.92,
                                      height: 1.3,
                                    ),
                                  ),
                              ],
                            ),
                          ),
                      ],
                    ),
            ),
          ),
          // 底部控制条：半透明
          Container(
            margin: const EdgeInsets.fromLTRB(6, 6, 6, 8),
            padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
            decoration: BoxDecoration(
              color: Colors.black.withValues(alpha: 0.55),
              borderRadius: BorderRadius.circular(8),
            ),
            child: Row(
              children: [
                IconButton(
                  visualDensity: VisualDensity.compact,
                  tooltip: state.running ? '暂停' : '开始',
                  icon: Icon(
                    state.running ? Icons.pause : Icons.play_arrow,
                    color: Colors.white,
                  ),
                  onPressed: () =>
                      state.running ? state.pause() : state.start(),
                ),
                IconButton(
                  visualDensity: VisualDensity.compact,
                  tooltip: '退出悬浮模式',
                  icon: const Icon(Icons.close, color: Colors.white),
                  onPressed: () => state.setFloatingMode(false),
                ),
                const Spacer(),
                // 字号微调（用户要求可调字号）
                IconButton(
                  visualDensity: VisualDensity.compact,
                  tooltip: '减小字号',
                  icon: const Icon(Icons.text_decrease, color: Colors.white),
                  onPressed: () =>
                      state.setSubtitleFontSize(state.subtitleFontSize - 2),
                ),
                Text('${state.subtitleFontSize.round()}',
                    style:
                        const TextStyle(color: Colors.white70, fontSize: 12)),
                IconButton(
                  visualDensity: VisualDensity.compact,
                  tooltip: '增大字号',
                  icon: const Icon(Icons.text_increase, color: Colors.white),
                  onPressed: () =>
                      state.setSubtitleFontSize(state.subtitleFontSize + 2),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

/// 悬浮窗拖动把手。
///
/// 原理：松手时给窗口发 `WM_NCLBUTTONDOWN + HTCAPTION`，Windows 会
/// 自动进入窗口拖动模态（跟手、松手结束）—— 不需要自己算鼠标位移。
/// 详见 `startDragFloatingWindow` 的注释。
class _FloatingDragHandle extends StatelessWidget {
  const _FloatingDragHandle({required this.state});

  final TranslatorState state;

  @override
  Widget build(BuildContext context) {
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onPanStart: (_) => startDragFloatingWindow(),
      child: Container(
        height: 26,
        alignment: Alignment.center,
        child: Container(
          width: 44,
          height: 4,
          decoration: BoxDecoration(
            color: Colors.white.withValues(alpha: 0.5),
            borderRadius: BorderRadius.circular(2),
          ),
        ),
      ),
    );
  }
}
