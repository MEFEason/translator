import 'dart:io' show exit;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show Clipboard, ClipboardData;

/// 独立错误窗口：程序发生未捕获异常时展示详细信息与堆栈。
///
/// 由 `main.dart` 的全局错误监控（`FlutterError.onError` /
/// `PlatformDispatcher.instance.onError`）弹出，靠全局 navigatorKey 取 context。
///
/// 提供三个动作：
/// - **复制日志**：把标题 + 信息 + 堆栈整份复制到剪贴板，便于反馈
/// - **继续运行**：关闭弹窗，尽力让程序带着已降级的功能继续跑
/// - **退出程序**：异常已不可恢复时直接结束进程
class ErrorDialogPage extends StatefulWidget {
  /// 弹窗标题（默认「程序异常」）
  final String? title;

  /// 错误主信息（必填）
  final String message;

  /// 详细信息 / 调用堆栈（可为空）
  final String? detail;

  const ErrorDialogPage({
    super.key,
    this.title,
    required this.message,
    this.detail,
  });

  @override
  State<ErrorDialogPage> createState() => _ErrorDialogPageState();
}

class _ErrorDialogPageState extends State<ErrorDialogPage> {
  bool _copied = false;

  /// 完整日志文本（复制用）
  String get _fullLog {
    final buf = StringBuffer(widget.title ?? '程序异常')
      ..write('\n')
      ..write(widget.message);
    final detail = widget.detail;
    if (detail != null && detail.isNotEmpty) {
      buf.write('\n\n$detail');
    }
    return buf.toString();
  }

  Future<void> _copyLog() async {
    await Clipboard.setData(ClipboardData(text: _fullLog));
    if (!mounted) return;
    setState(() => _copied = true);
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final detail = widget.detail;

    return AlertDialog(
      title: Row(
        children: [
          Icon(Icons.error_outline, color: theme.colorScheme.error),
          const SizedBox(width: 8),
          Expanded(child: Text(widget.title ?? '程序异常')),
        ],
      ),
      content: SizedBox(
        width: 560,
        child: SingleChildScrollView(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              SelectableText(widget.message),
              if (detail != null && detail.isNotEmpty) ...[
                const SizedBox(height: 12),
                const Text(
                  '详细信息 / 堆栈',
                  style: TextStyle(fontWeight: FontWeight.bold, fontSize: 12),
                ),
                const SizedBox(height: 4),
                Container(
                  width: double.infinity,
                  constraints: const BoxConstraints(maxHeight: 220),
                  padding: const EdgeInsets.all(8),
                  decoration: BoxDecoration(
                    // 不依赖新增的 ColorScheme 字段，兼容深浅色主题
                    color: Colors.grey.withValues(alpha: 0.15),
                    borderRadius: BorderRadius.circular(6),
                  ),
                  child: SingleChildScrollView(
                    child: SelectableText(
                      detail,
                      style: const TextStyle(
                        fontSize: 11,
                        fontFamily: 'monospace',
                      ),
                    ),
                  ),
                ),
              ],
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: _copied ? null : _copyLog,
          child: Text(_copied ? '已复制' : '复制日志'),
        ),
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('继续运行'),
        ),
        FilledButton(
          onPressed: () => exit(1),
          child: const Text('退出程序'),
        ),
      ],
    );
  }
}
