import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:path/path.dart' as p;
import 'package:provider/provider.dart';

import '../services/responsive.dart';
import '../services/model_paths.dart';
import '../state/translator_state.dart';

/// 词库管理页。
///
/// 词库是本产品核心差异化（需求 5）—— 术语表通过三层机制同时作用于
/// ASR 热词、翻译前哨兵保护、翻译后强制替换。支持格式见 docs/GLOSSARY.md。
///
/// ⚠️ 2026-10-25 重构（用户：「生成词库并内置，保留用户自行导入的入口，
/// 并提供示例文件」）：
/// 旧页永远显示「尚未导入词库」—— 因为内置词库在 bootstrap 已随会话
/// 加载，页面却从不查询真实词条数。现在：
/// - 显示内置词库的真实状态与词条数（`bridge.glossaryCount()`）
/// - **保留**导入入口（用户 TSV 是追加，不覆盖内置）
/// - 提供「示例词库文件」：复制到转写目录并可在资源管理器打开，
///   用户照格式填好再从这里导回
class GlossaryPage extends StatefulWidget {
  const GlossaryPage({super.key});

  @override
  State<GlossaryPage> createState() => _GlossaryPageState();
}

class _GlossaryPageState extends State<GlossaryPage> {
  /// 当前生效词条总数；null = 还没查到
  int? _count;

  @override
  void initState() {
    super.initState();
    Future.microtask(_refresh);
  }

  void _refresh() {
    final state = context.read<TranslatorState>();
    final n = state.bridge.glossaryCount();
    if (mounted) setState(() => _count = n >= 0 ? n : null);
  }

  /// 把示例词库写到转写目录（用户改起来方便），并在资源管理器定位。
  Future<void> _openExample(BuildContext context) async {
    final state = context.read<TranslatorState>();
    try {
      final dir = Directory(state.transcriptPath);
      if (!dir.existsSync()) dir.createSync(recursive: true);
      final dst = File(p.join(dir.path, 'example-glossary.tsv'));
      // 示例文件源：安装目录 models/glossary 下（随包分发）
      // ⚠️ 用 resolveModelPaths 而不是 bridge.discoverModels()：
      //    后者在 Android 上必然返回空（Rust 只探 exe 相邻目录）。
      final models = resolveModelPaths(state);
      final src = models.root.isEmpty
          ? null
          : File(p.join(models.root, 'glossary', 'example-glossary.tsv'));
      if (src != null && src.existsSync()) {
        await src.copy(dst.path);
      } else {
        // 兜底：模型目录异常时写一份最小可用样例
        await dst.writeAsString(
          'source\ttarget\taliases\tscope\tgroup\n'
          'total quality management\t全面质量管理\tTQM\tboth\t示例\n',
        );
      }
      if (!mounted) return;
      // 打开所在文件夹让用户编辑（资源管理器会选中该文件）
      await Process.run('explorer.exe', ['/select,', dst.path]);
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('示例已放到转写目录：${dst.path}\n改好后回到这里点「导入词库」')),
      );
    } catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('生成示例失败：$e')),
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final loaded = _count != null && _count! > 0;

    return Scaffold(
      appBar: AppBar(title: const Text('词库')),
      body: ResponsiveBody(
        child: ListView(
          padding: const EdgeInsets.all(16),
          children: [
            // ── 当前状态卡 ──
            Card(
              margin: EdgeInsets.zero,
              child: ListTile(
                leading: Icon(
                  loaded ? Icons.check_circle : Icons.info_outline,
                  color: loaded ? Colors.green : theme.hintColor,
                ),
                title: Text(loaded ? '词库已生效' : '词库加载中…',
                    style: const TextStyle(fontSize: 14)),
                subtitle: Text(
                  loaded
                      ? '共 $_count 个词条（内置课程术语 + 你导入的文件），'
                          '同时作用于识别与翻译'
                      : '会话尚未就绪（自检完成后自动显示词条数）',
                  style: const TextStyle(fontSize: 11),
                ),
              ),
            ),
            const SizedBox(height: 8),
            Card(
              margin: EdgeInsets.zero,
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text('工作原理', style: theme.textTheme.titleSmall),
                    const SizedBox(height: 6),
                    Text(
                      '识别时按你的写法出词（"TQM" 不会被听成 "T Q M"）；\n'
                      '翻译时术语不会被机器翻译乱改，保证全篇统一。',
                      style: theme.textTheme.bodySmall
                          ?.copyWith(color: theme.hintColor, fontSize: 11),
                    ),
                  ],
                ),
              ),
            ),
            const SizedBox(height: 16),
            // ── 用户操作区（导入入口保留）──
            FilledButton.icon(
              onPressed: () => _import(context),
              icon: const Icon(Icons.file_upload),
              label: const Text('导入我的词库'),
            ),
            const SizedBox(height: 8),
            OutlinedButton.icon(
              onPressed: () => _openExample(context),
              icon: const Icon(Icons.description_outlined, size: 18),
              label: const Text('获取示例词库文件'),
            ),
            const SizedBox(height: 8),
            Text(
              '导入的词条是**追加**（与内置术语合并，可覆盖同名译法）。\n'
              '支持 .tsv / .csv / .json / .yaml / .txt（每行"英文=中文"）。\n'
              '内置术语来源：讲师康奈尔笔记自动生成的课程词库，如需调整'
              '可直接导入同名术语覆盖。',
              style: TextStyle(fontSize: 10, color: theme.hintColor),
            ),
          ],
        ),
      ),
    );
  }

  /// 导入词库文件。
  ///
  /// 选中的是**文件绝对路径** —— 解析在 Rust 侧完成（`translator_glossary_load`），
  /// Dart 侧不碰词库格式。这样五种格式（tsv/csv/json/yaml/plain）的解析规则
  /// 只有一份，三端一致。
  Future<void> _import(BuildContext context) async {
    final result = await FilePicker.platform.pickFiles(
      allowMultiple: true,
      type: FileType.custom,
      allowedExtensions: const ['tsv', 'csv', 'json', 'yaml', 'yml', 'txt'],
    );
    if (result == null || result.files.isEmpty) return;

    final state = context.read<TranslatorState>();
    var ok = 0;
    var fail = 0;
    var added = 0;
    for (final f in result.files) {
      final path = f.path;
      if (path == null) {
        fail += 1;
        continue;
      }
      // 返回值 >=0 是词条数，<0 是错误码
      final n = state.bridge.glossaryLoad(path);
      if (n >= 0) {
        ok += 1;
        added += n;
      } else {
        fail += 1;
      }
    }
    if (!context.mounted) return;
    _refresh();
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        content: Text('已导入 $ok 个文件共 $added 条'
            '${fail > 0 ? '，$fail 个失败' : ''}'),
      ),
    );
  }
}
