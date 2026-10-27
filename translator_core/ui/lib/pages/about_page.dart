import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/translator_state.dart';
import 'channel_info_page.dart';

/// 版本渠道说明（沿用 Maven 的版本语义）。
///
/// 渠道决定这个构建**处于什么阶段**，以及用户该不该在正式场合用。
/// 版本号里的渠道前缀就是这里的 key。
class ChannelInfo {
  final String name;

  /// 详细说明（放在独立页面）
  final String desc;

  /// 一句话摘要（放在关于页，避免 7 段说明堆成一堵字墙）
  final String brief;

  /// 是否推荐在正式场合使用（只影响图标，不写「不建议使用」这类
  /// 面向开发者的判断 —— 那是给构建者看的，不是给用户看的）
  final bool recommended;

  const ChannelInfo(
    this.name,
    this.desc,
    this.brief, {
    this.recommended = false,
  });

  static const Map<String, ChannelInfo> all = {
    'snapshot': ChannelInfo(
      '快照（开发版）',
      '开发过程中的版本，功能会不时更新，正式环境中请不要使用。',
      '开发版，功能会变化，正式场合勿用',
    ),
    'alpha': ChannelInfo(
      '内部测试版',
      '较早的测试版本，主要供开发与测试人员测试、查找问题。',
      '内部测试版，供开发测试找 BUG',
    ),
    'beta': ChannelInfo(
      '公开测试版',
      '比内部测试版更进一步的版本，面向公众测试。',
      '公开测试版，面向公众测试',
    ),
    'pre': ChannelInfo(
      '预发布版',
      '与内部测试版类似，有时还会细分为 M1、M2 等版本。',
      '预发布版，类似 alpha',
    ),
    'release': ChannelInfo(
      '发行版',
      '官方推荐使用的版本，也就是我们经常下载的版本。',
      '官方推荐使用的版本',
      recommended: true,
    ),
    'stable': ChannelInfo(
      '稳定版',
      '相比测试版更加稳定，去除了许多问题，完善了一些功能。',
      '稳定版，缺陷更少、功能更完善',
      recommended: true,
    ),
    'current': ChannelInfo(
      '最新版',
      '最新的版本，但不一定是稳定版本，需确认是否还有发行版或稳定版可选。',
      '最新版，不一定是稳定版',
    ),
  };

  /// 从版本串解析渠道：`beta-v0.9.1-build2026.1022.15.16` → `beta`
  static String channelOf(String version) {
    final m = RegExp(r'^([a-z]+)-').firstMatch(version);
    return m?.group(1) ?? '';
  }
}

/// 关于页。
///
/// 展示**构建时生成**的版本号 —— 用户报障时能凭它定位到具体构建。
/// 版本串由 Rust 侧 `translator_version()` 提供，形如：
/// `beta-v0.9.1-build2026.1022.15.16`
/// 四段分别是：渠道 + 语义版本（v主.次.修订）+ 构建号（build+时间）。
class AboutPage extends StatelessWidget {
  const AboutPage({super.key});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final state = context.watch<TranslatorState>();
    final version = state.bridge.version();
    final channel = ChannelInfo.all[ChannelInfo.channelOf(version)];

    return Scaffold(
      appBar: AppBar(title: const Text('关于')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          Center(
            child: Column(
              children: [
                const Icon(Icons.translate, size: 56),
                const SizedBox(height: 12),
                Text('Translator',
                    style: theme.textTheme.headlineSmall
                        ?.copyWith(fontWeight: FontWeight.w600)),
                const SizedBox(height: 4),
                Text('离线实时转写翻译',
                    style: theme.textTheme.bodyMedium
                        ?.copyWith(color: theme.hintColor)),
              ],
            ),
          ),
          const SizedBox(height: 24),

          // ── 版本 ──
          Card(
            margin: EdgeInsets.zero,
            child: Column(
              children: [
                ListTile(
                  leading: const Icon(Icons.tag),
                  title: const Text('版本'),
                  subtitle: SelectableText(
                    version.isEmpty ? '未知' : version,
                    style: const TextStyle(fontFamily: 'monospace'),
                  ),
                ),
                if (channel != null) ...[
                  const Divider(height: 1),
                  ListTile(
                    dense: true,
                    leading: Icon(
                      channel.recommended
                          ? Icons.verified_outlined
                          : Icons.build_circle_outlined,
                      size: 18,
                      color: channel.recommended
                          ? Colors.green.shade600
                          : theme.colorScheme.error,
                    ),
                    title: Text(channel.name),
                    // 只放一句话摘要 —— 详细解释在独立页面
                    subtitle: Text(channel.brief),
                    trailing: TextButton(
                      onPressed: () => Navigator.of(context).push(
                        MaterialPageRoute(
                            builder: (_) => const ChannelInfoPage()),
                      ),
                      child: const Text('详情'),
                    ),
                  ),
                ],
                const Divider(height: 1),
                const ListTile(
                  dense: true,
                  leading: Icon(Icons.wifi_off, size: 18),
                  title: Text('完全离线运行'),
                  subtitle: Text('不联网、不上传、无遥测'),
                ),
              ],
            ),
          ),
          const SizedBox(height: 16),

          // ── 组件状态 ──
          Card(
            margin: EdgeInsets.zero,
            child: Column(
              children: [
                const ListTile(
                  dense: true,
                  leading: Icon(Icons.check_circle_outline, size: 18),
                  title: Text('组件状态'),
                ),
                const Divider(height: 1),
                ...state.selfcheckSteps.map(
                  (s) => ListTile(
                    dense: true,
                    leading: Icon(
                      s.ok ? Icons.check_circle : Icons.error_outline,
                      size: 18,
                      color: s.ok
                          ? Colors.green.shade600
                          : theme.colorScheme.error,
                    ),
                    title: Text(s.name),
                    trailing: Text(s.detail,
                        style: theme.textTheme.bodySmall
                            ?.copyWith(color: theme.hintColor)),
                  ),
                ),
              ],
            ),
          ),
          const SizedBox(height: 16),

          // ── 数据位置 ──
          Card(
            margin: EdgeInsets.zero,
            child: Column(
              children: [
                const ListTile(
                  dense: true,
                  leading: Icon(Icons.folder_outlined, size: 18),
                  title: Text('数据位置'),
                ),
                const Divider(height: 1),
                ListTile(
                  dense: true,
                  title: const Text('转写盘'),
                  // 语义纠正（2026-10-25）：`transcriptPath` 是**目录**，
                  // 以前被 `File(...).parent.path` 当成文件路径取了父目录，
                  // 于是显示成「桌面」而看不到真正的 translate results。
                  subtitle: Text(
                    state.transcriptPath.isEmpty
                        ? '未启用'
                        : '${state.transcriptPath}'
                            '${state.isDefaultTranscriptDir ? '（默认位置）' : '（自定义）'}',
                  ),
                  trailing: state.transcriptPath.isEmpty
                      ? null
                      : TextButton(
                          onPressed: () => _changeTranscriptDir(context, state),
                          child: const Text('更改'),
                        ),
                ),
                if (state.transcriptPath.isNotEmpty)
                  Padding(
                    padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
                    child: Row(
                      children: [
                        Expanded(
                          child: Text(
                            '默认位置是桌面下的 translate results 文件夹。'
                            '更改位置对下一场转写生效，并会被记住。',
                            style: theme.textTheme.bodySmall
                                ?.copyWith(color: theme.hintColor),
                          ),
                        ),
                        if (!state.isDefaultTranscriptDir)
                          TextButton(
                            onPressed: () => state.resetTranscriptDir(),
                            child: const Text('恢复默认'),
                          ),
                      ],
                    ),
                  ),
              ],
            ),
          ),
          const SizedBox(height: 24),

          Center(
            child: Text(
              '本软件完全离线运行，不发起任何网络请求。\n'
              '翻译结果仅供参考，重要内容请自行核对。',
              textAlign: TextAlign.center,
              style:
                  theme.textTheme.bodySmall?.copyWith(color: theme.hintColor),
            ),
          ),
        ],
      ),
    );
  }
}

/// 让用户选择新的转写盘位置。
///
/// 选完即持久化（`TranslatorState.setTranscriptDir`），
/// 对下一场转写生效，重启应用后仍记得。
Future<void> _changeTranscriptDir(
    BuildContext context, TranslatorState state) async {
  final picked = await FilePicker.platform.getDirectoryPath(
    dialogTitle: '选择转写盘位置',
    initialDirectory:
        state.transcriptPath.isNotEmpty ? state.transcriptPath : null,
  );
  if (picked == null || picked.isEmpty) return;
  await state.setTranscriptDir(picked);
  if (!context.mounted) return;
  ScaffoldMessenger.of(context).showSnackBar(
    const SnackBar(content: Text('转写盘位置已更改，下一场转写生效')),
  );
}
