import 'package:flutter/material.dart';

import 'about_page.dart';

/// 版本渠道说明页（独立页面）。
///
/// 关于页只放一行简明摘要 + 入口 —— 7 个渠道的详细解释有 20 多行，
/// 塞进去会把「关于本应用」挤成一堵字墙。
class ChannelInfoPage extends StatelessWidget {
  const ChannelInfoPage({super.key});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);

    return Scaffold(
      appBar: AppBar(title: const Text('版本渠道说明')),
      body: ListView(
        padding: const EdgeInsets.symmetric(vertical: 8),
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 8, 16, 12),
            child: Text(
              '版本号的前缀表示这个构建处于什么阶段。'
              '测试类版本不建议在正式场合使用。',
              style:
                  theme.textTheme.bodySmall?.copyWith(color: theme.hintColor),
            ),
          ),
          for (final c in ChannelInfo.all.values)
            Card(
              margin: const EdgeInsets.fromLTRB(12, 0, 12, 8),
              child: ListTile(
                title: Row(
                  children: [
                    Icon(
                      c.recommended
                          ? Icons.verified_outlined
                          : Icons.build_circle_outlined,
                      size: 18,
                      color: c.recommended
                          ? Colors.green.shade600
                          : theme.colorScheme.error,
                    ),
                    const SizedBox(width: 8),
                    Text(c.name,
                        style: const TextStyle(
                            fontWeight: FontWeight.w600, fontSize: 14)),
                  ],
                ),
                subtitle: Padding(
                  padding: const EdgeInsets.only(top: 4),
                  child: Text(c.desc,
                      style: const TextStyle(fontSize: 12, height: 1.4)),
                ),
              ),
            ),
          const SizedBox(height: 16),
        ],
      ),
    );
  }
}
