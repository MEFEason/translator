import 'dart:io';

import 'package:flutter/material.dart';
import 'package:path/path.dart' as p;
import 'package:provider/provider.dart';

import '../ffi/bridge.dart';
import '../services/responsive.dart';
import '../state/translator_state.dart';

/// 模型管理页。
///
/// ## 2026-10-25 重构（用户要求「参考大厂」）
///
/// **移除导入按钮与 FAB**。原实现的问题：
/// - 三个「导入」按钮是**空回调**（`onPressed: () {}`），点了没有任何反应
/// - `installed` 全是硬编码 `false`，模型名/大小也是过时写死的
///   （写着 `zipformer-en-256ms · 72 MB`，实际早已换成 x-asr-960ms）
///
/// 本应用的模型**随安装包分发**（离线硬约束，运行期不下载），
/// 运行期导入不是必要功能。改为展示**真实状态**：
/// 目录路径 + 实际占用 + 是否就绪，并提供「打开模型文件夹」。
class ModelPage extends StatefulWidget {
  const ModelPage({super.key});

  @override
  State<ModelPage> createState() => _ModelPageState();
}

class _ModelPageState extends State<ModelPage> {
  ModelPaths _paths = const ModelPaths('', '', '');

  /// 各模型目录的实际占用（key = 目录路径）
  final Map<String, int> _bytes = {};

  @override
  void initState() {
    super.initState();
    Future.microtask(_load);
  }

  Future<void> _load() async {
    final state = context.read<TranslatorState>();
    final paths = state.bridge.discoverModels();
    if (!mounted) return;
    setState(() => _paths = paths);
    // 异步计算各目录占用（大目录遍历有耗时，不能卡首帧）
    final dirs = <String>[
      paths.asr,
      paths.mt,
      if (_offlineDir.isNotEmpty) _offlineDir,
    ].where((d) => d.isNotEmpty).toList();
    for (final d in dirs) {
      final size = await _dirSize(d);
      if (!mounted) return;
      setState(() => _bytes[d] = size);
    }
  }

  /// 离线精修模型目录（两遍解码第二遍）
  String get _offlineDir {
    if (_paths.root.isEmpty) return '';
    return '${_paths.root}${p.separator}offline${p.separator}sense-voice-int8';
  }

  /// 递归统计目录占用（失败/不存在返回 0）
  Future<int> _dirSize(String path) async {
    try {
      final dir = Directory(path);
      if (!dir.existsSync()) return 0;
      int total = 0;
      await for (final e in dir.list(recursive: true, followLinks: false)) {
        if (e is File) {
          try {
            total += await e.length();
          } catch (_) {}
        }
      }
      return total;
    } catch (_) {
      return 0;
    }
  }

  String _fmt(int bytes) {
    const mb = 1024 * 1024;
    if (bytes >= mb) return '${(bytes / mb).toStringAsFixed(0)} MB';
    if (bytes >= 1024) return '${(bytes / 1024).toStringAsFixed(0)} KB';
    return '';
  }

  Future<void> _openFolder() async {
    final root = _paths.root;
    if (root.isEmpty) return;
    try {
      await Process.start('explorer.exe', [root]);
    } catch (_) {
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('无法打开文件夹：$root')),
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final offlineDir = _offlineDir;

    return Scaffold(
      appBar: AppBar(title: const Text('模型')),
      body: ResponsiveBody(
        child: ListView(
          padding: const EdgeInsets.all(16),
          children: [
            _ModelCard(
              icon: Icons.graphic_eq,
              role: '语音识别（ASR）',
              model: 'x-asr-960ms int8 · 中英 · 自带标点',
              path: _paths.asr,
              sizeText: _fmt(_bytes[_paths.asr] ?? 0),
            ),
            const SizedBox(height: 8),
            _ModelCard(
              icon: Icons.translate,
              role: '离线翻译（MT）',
              model: 'Bergamot en-zh',
              path: _paths.mt,
              sizeText: _fmt(_bytes[_paths.mt] ?? 0),
            ),
            const SizedBox(height: 8),
            _ModelCard(
              icon: Icons.auto_fix_high,
              role: '离线精修（第二遍解码）',
              model: 'SenseVoice int8 · 中英日韩粤',
              path: offlineDir,
              sizeText: _fmt(_bytes[offlineDir] ?? 0),
              optional: true,
            ),
            const SizedBox(height: 8),
            const _ModelCard(
              icon: Icons.hearing,
              role: '语音活动检测（VAD）',
              model: 'Silero VAD',
              path: '',
              sizeText: '已内置',
              builtin: true,
            ),
            const SizedBox(height: 16),
            FilledButton.icon(
              onPressed: _paths.root.isEmpty ? null : _openFolder,
              icon: const Icon(Icons.folder_open),
              label: const Text('打开模型文件夹'),
            ),
            const SizedBox(height: 16),
            Card(
              margin: EdgeInsets.zero,
              color: theme.colorScheme.surfaceContainerHighest,
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Row(
                      children: [
                        Icon(Icons.cloud_off, size: 14, color: theme.hintColor),
                        const SizedBox(width: 6),
                        Text('完全离线', style: theme.textTheme.bodySmall),
                      ],
                    ),
                    const SizedBox(height: 6),
                    Text(
                      '本应用不联网、不上传、无遥测，模型随安装包一同分发。\n'
                      '「离线精修」为可选项：缺失时自动使用单遍识别，不影响使用。',
                      style: theme.textTheme.bodySmall?.copyWith(
                        color: theme.hintColor,
                        fontSize: 11,
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// 单个模型的状态卡片（大厂风格：状态 + 名称 + 路径 + 占用）。
class _ModelCard extends StatelessWidget {
  final IconData icon;
  final String role;
  final String model;

  /// 模型目录（空 = 内置，无独立目录）
  final String path;
  final String sizeText;

  /// 可选模型：缺失时显示「未启用」而非「缺失」
  final bool optional;

  /// 内置于核心（VAD）：永远算就绪
  final bool builtin;

  const _ModelCard({
    required this.icon,
    required this.role,
    required this.model,
    required this.path,
    required this.sizeText,
    this.optional = false,
    this.builtin = false,
  });

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final exists = path.isNotEmpty && Directory(path).existsSync();
    final ready = builtin || exists;

    // 三态：就绪（绿）/ 可选但未装（灰）/ 缺失（红）
    final Color statusColor;
    final IconData statusIcon;
    final String statusText;
    if (ready) {
      statusColor = Colors.green;
      statusIcon = Icons.check_circle;
      statusText = builtin ? '已内置' : '已就绪';
    } else if (optional) {
      statusColor = theme.hintColor;
      statusIcon = Icons.remove_circle_outline;
      statusText = '未启用';
    } else {
      statusColor = theme.colorScheme.error;
      statusIcon = Icons.error_outline;
      statusText = '缺失';
    }

    return Card(
      margin: EdgeInsets.zero,
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(icon, size: 20, color: theme.colorScheme.primary),
            const SizedBox(width: 10),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Row(
                    children: [
                      Text(role, style: theme.textTheme.titleSmall),
                      const SizedBox(width: 8),
                      if (optional && !ready)
                        Container(
                          padding: const EdgeInsets.symmetric(
                              horizontal: 6, vertical: 1),
                          decoration: BoxDecoration(
                            color: theme.colorScheme.surfaceContainerHighest,
                            borderRadius: BorderRadius.circular(4),
                          ),
                          child: Text(
                            '可选',
                            style: theme.textTheme.labelSmall?.copyWith(
                              color: theme.hintColor,
                            ),
                          ),
                        ),
                    ],
                  ),
                  const SizedBox(height: 2),
                  Text(
                    model,
                    style: theme.textTheme.bodySmall
                        ?.copyWith(color: theme.hintColor),
                  ),
                  if (exists) ...[
                    const SizedBox(height: 4),
                    Text(
                      path,
                      style: TextStyle(
                        fontSize: 10,
                        color: theme.hintColor,
                        fontFamily: 'monospace',
                      ),
                      maxLines: 2,
                      overflow: TextOverflow.ellipsis,
                    ),
                  ],
                ],
              ),
            ),
            const SizedBox(width: 8),
            Column(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                Row(
                  children: [
                    Icon(statusIcon, size: 14, color: statusColor),
                    const SizedBox(width: 4),
                    Text(
                      statusText,
                      style: theme.textTheme.labelSmall
                          ?.copyWith(color: statusColor),
                    ),
                  ],
                ),
                if (sizeText.isNotEmpty) ...[
                  const SizedBox(height: 2),
                  Text(sizeText, style: theme.textTheme.bodySmall),
                ],
              ],
            ),
          ],
        ),
      ),
    );
  }
}
