import 'dart:io';

import 'package:flutter/material.dart';

import 'about_page.dart';
import 'package:provider/provider.dart';

import '../ffi/bridge.dart';
import '../services/responsive.dart';
import '../services/tray_service.dart';
import '../state/translator_state.dart';

/// 设置 / 环境页。
///
/// ## 为什么这一页最重要
///
/// 采集能力不可用时（卓易通容器 / DRM / 未授权），用户必须知道
/// **为什么**、**怎么办**。静默降级到麦克风会让用户误判程序故障。
///
/// 卓易通特有引导（DESIGN §2.5）：
/// - 容器内需在卓易通「应用信息」页逐项授权
/// - 存储隔离 → 模型/词库须经「文件互传」导入
/// - 建议将卓易通加入电池优化白名单，避免容器休眠断流
class CapabilityPage extends StatelessWidget {
  const CapabilityPage({super.key});

  @override
  Widget build(BuildContext context) {
    final state = context.watch<TranslatorState>();
    final theme = Theme.of(context);

    return Scaffold(
      appBar: AppBar(title: const Text('设置')),
      body: ResponsiveBody(
        child: ListView(
          padding: const EdgeInsets.all(16),
          children: [
            _Section(title: '采集能力', children: [
              _CapabilityTile(state: state),
            ]),
            const SizedBox(height: 16),
            _Section(title: '窗口与通知栏', children: [
              // 字号滑块（悬浮字幕可调，16~40）
              ListTile(
                dense: true,
                leading: const Icon(Icons.format_size),
                title: const Text('字幕字号'),
                subtitle: Row(
                  children: [
                    const Text('A', style: TextStyle(fontSize: 11)),
                    Expanded(
                      child: Slider(
                        value: state.subtitleFontSize,
                        min: 16,
                        max: 40,
                        divisions: 12,
                        label: '${state.subtitleFontSize.round()}',
                        onChanged: state.setSubtitleFontSize,
                      ),
                    ),
                    const Text('A', style: TextStyle(fontSize: 20)),
                  ],
                ),
                trailing: Text('${state.subtitleFontSize.round()}'),
              ),
              // 独立歌词窗（第二实例，QQ 音乐桌面歌词风格）
              SwitchListTile(
                dense: true,
                secondary: const Icon(Icons.lyrics_outlined),
                title: const Text('独立字幕窗'),
                subtitle: const Text(
                  '另开一个无边框置顶小窗显示双语字幕（桌面歌词风格）：'
                  '可整窗拖动、拖边角调整大小（大小自动记住）；与主窗口并存',
                  style: TextStyle(fontSize: 11),
                ),
                value: state.lyricsWindowEnabled,
                onChanged: (v) => state.setLyricsWindowEnabled(v),
              ),
              // 关闭窗口时的行为（2026-10-25 用户要求「参考大厂」）
              //
              // ⚠️ 2026-10-28 用户：这是 **Windows 端**的交互。
              // Android 上点 ✕ 走系统默认（Activity 退到后台）+ 常驻通知，
              // 不做「询问 / 最小化 / 退出」三选一 —— 那是桌面托盘的产品形态，
              // 手机上弹窗询问反而是倒退。故整块仅桌面端显示。
              if (!Platform.isAndroid) ...[
                RadioListTile<String>(
                  dense: true,
                  secondary: const Icon(Icons.help_outline),
                  title: const Text('关闭时询问'),
                  subtitle: const Text(
                    '点 ✕ 弹出选择框：最小化到通知栏，还是退出程序（推荐）',
                    style: TextStyle(fontSize: 11),
                  ),
                  value: 'ask',
                  // ignore: deprecated_member_use
                  groupValue: state.closeAction,
                  // ignore: deprecated_member_use
                  onChanged: (v) {
                    if (v != null) state.setCloseAction(v);
                  },
                ),
                RadioListTile<String>(
                  dense: true,
                  secondary: const Icon(Icons.notifications_active_outlined),
                  title: const Text('最小化到通知栏'),
                  subtitle: const Text(
                    '点 ✕ 不退出，缩到通知栏继续转写；左键点图标恢复窗口，右键出菜单',
                    style: TextStyle(fontSize: 11),
                  ),
                  value: 'minimize',
                  // ignore: deprecated_member_use
                  groupValue: state.closeAction,
                  // ignore: deprecated_member_use
                  onChanged: (v) {
                    if (v != null) state.setCloseAction(v);
                  },
                ),
                RadioListTile<String>(
                  dense: true,
                  secondary: const Icon(Icons.close),
                  title: const Text('退出程序'),
                  subtitle: const Text(
                    '点 ✕ 走退出流程：先显示「正在保存」，保存完成后退出，'
                    '与托盘「退出程序」一致',
                    style: TextStyle(fontSize: 11),
                  ),
                  value: 'quit',
                  // ignore: deprecated_member_use
                  groupValue: state.closeAction,
                  // ignore: deprecated_member_use
                  onChanged: (v) {
                    if (v != null) state.setCloseAction(v);
                  },
                ),
              ] else
                // Android：常驻通知是唯一的「后台保活」说明。
                ListTile(
                  dense: true,
                  leading: const Icon(Icons.notifications_active_outlined),
                  title: const Text('后台运行'),
                  subtitle: const Text(
                    '转写期间会显示常驻通知（前台服务），点 ✕ 退出主界面后'
                    '仍在后台继续转写，可从通知栏的「停止转写」结束',
                    style: TextStyle(fontSize: 11),
                  ),
                ),
              // 「显示主窗口」是 Windows 托盘的恢复入口 —— Android 无托盘，
              // 从桌面图标/最近任务即可回到主界面，故整项仅桌面端显示。
              if (!Platform.isAndroid)
                ListTile(
                  dense: true,
                  leading: const Icon(Icons.open_in_new),
                  title: const Text('显示主窗口'),
                  subtitle: const Text(
                    '窗口已隐藏到通知栏时，从这里恢复',
                    style: TextStyle(fontSize: 11),
                  ),
                  onTap: () => TrayService.instance.showWindow(),
                ),
            ]),
            const SizedBox(height: 16),
            _Section(title: '声源（可多选，同时转译）', children: [
              SwitchListTile(
                dense: true,
                secondary: const Icon(Icons.volume_up),
                title: const Text('系统内放'),
                subtitle: Text('对方说的 · 说话人「${state.speakerRemote}」'),
                value: state.sources.contains('systemAudio'),
                onChanged: (v) => state.toggleSource('systemAudio', v),
              ),
              SwitchListTile(
                dense: true,
                secondary: const Icon(Icons.mic),
                title: const Text('麦克风'),
                subtitle: Text('自己说的 · 说话人「${state.speakerLocal}」'),
                value: state.sources.contains('microphone'),
                onChanged: (v) => state.toggleSource('microphone', v),
              ),
              ListTile(
                dense: true,
                leading: const Icon(Icons.label_outline),
                title: const Text('说话人标签'),
                subtitle: const Text('自定义字幕上显示的名字'),
                trailing: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    SizedBox(
                      width: 72,
                      child: TextField(
                        controller:
                            TextEditingController(text: state.speakerRemote),
                        decoration: const InputDecoration(
                            isDense: true, labelText: '对方'),
                        onSubmitted: (v) =>
                            state.setSpeakerLabel('systemAudio', v),
                      ),
                    ),
                    const SizedBox(width: 8),
                    SizedBox(
                      width: 72,
                      child: TextField(
                        controller:
                            TextEditingController(text: state.speakerLocal),
                        decoration: const InputDecoration(
                            isDense: true, labelText: '我'),
                        onSubmitted: (v) =>
                            state.setSpeakerLabel('microphone', v),
                      ),
                    ),
                  ],
                ),
              ),
              const Padding(
                padding: EdgeInsets.symmetric(horizontal: 16, vertical: 4),
                child: Text(
                  '两路同时转译时各自独立断句 —— 对方说话不会打断你的句子。',
                  style: TextStyle(fontSize: 11),
                ),
              ),
            ]),
            _Section(title: '运行环境', children: [
              ListTile(
                dense: true,
                leading: const Icon(Icons.memory),
                title: const Text('运行时'),
                // 「卓易通」是 Android 容器特有概念，桌面端不存在，
                // 继续显示会让用户以为有得选。
                subtitle: Text(
                  Platform.isAndroid
                      ? 'Android 原生 / 卓易通容器'
                      : '桌面原生（WASAPI 内放 / CoreAudio taps）',
                ),
                trailing: Platform.isAndroid
                    ? DropdownButton<String>(
                        value: state.runtime,
                        onChanged: (v) => state.setRuntime(v ?? 'native'),
                        items: const [
                          DropdownMenuItem(value: 'native', child: Text('原生')),
                          DropdownMenuItem(
                              value: 'zhuoyitong', child: Text('卓易通')),
                        ],
                      )
                    : const Chip(label: Text('桌面')),
              ),
              ListTile(
                dense: true,
                leading: const Icon(Icons.bolt),
                title: const Text('性能档位'),
                subtitle: Text(state.profile),
                trailing: SegmentedButton<String>(
                  segments: const [
                    ButtonSegment(value: 'stable', label: Text('稳定')),
                    ButtonSegment(value: 'fast', label: Text('快速')),
                  ],
                  selected: {state.profile},
                  onSelectionChanged: (s) => state.setProfile(s.first),
                ),
              ),
            ]),
            const SizedBox(height: 16),
            _Section(
              title: '资源占用（按其他软件的运行情况选择）',
              children: [
                _ResourceTierTile(state: state),
              ],
            ),
            if (state.isZhuoYiTong) ...[
              const SizedBox(height: 16),
              _ZhuoYiTongGuide(),
            ],
            const SizedBox(height: 16),
            _Section(title: '诊断', children: [
              ListTile(
                dense: true,
                leading: const Icon(Icons.refresh),
                title: const Text('重新检测采集能力'),
                onTap: state.probeCapability,
              ),
              ListTile(
                dense: true,
                leading: const Icon(Icons.bug_report),
                title: const Text('丢帧统计'),
                subtitle: Text('${state.droppedFrames} 帧'),
              ),
            ]),
            const SizedBox(height: 16),
            // 「关于」入口 —— 版本号、组件状态、数据位置都在里面
            Card(
              margin: EdgeInsets.zero,
              child: ListTile(
                leading: const Icon(Icons.info_outline),
                title: const Text('关于'),
                subtitle: Text(
                  state.bridge.version().isEmpty
                      ? 'Translator'
                      : state.bridge.version(),
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 11),
                ),
                trailing: const Icon(Icons.chevron_right),
                onTap: () => Navigator.of(context).push(
                  MaterialPageRoute(builder: (_) => const AboutPage()),
                ),
              ),
            ),
            const SizedBox(height: 24),
            Text(
              'Translator · 完全离线运行\n'
              '不联网、不上传、无遥测',
              textAlign: TextAlign.center,
              style:
                  theme.textTheme.bodySmall?.copyWith(color: theme.hintColor),
            ),
          ],
        ),
      ),
    );
  }
}

/// 按识别线程数给一句占用/场景说明（Slider 旁的提示）。
///
/// 2026-10-25 依据实测数据（每线程 ≈1.6 核真实功耗 / +235MB）：
String _tierAdvice(int threads) {
  if (threads <= 1) return '最低占用：同时跑游戏、大型编译等重负载时选它';
  if (threads <= 2) return '省资源：已能跟上实时语音（实测 RTF≈1.0）';
  if (threads <= 4) return '默认推荐：快语速连讲也稳，不和会议软件抢 CPU';
  if (threads <= 7) return '高占用：仅当 4 线程仍跟不上时使用（质量不会更高）';
  return '全力：8 线程 ≈ 63% 系统 CPU / 2GB 内存，质量与 4 线程完全相同';
}

/// 资源占用档位卡片。
///
/// ## 为什么要有这个
///
/// 翻译器要跑 ASR + MT 推理，必然吃 CPU。用户机器上常同时开着
/// 视频会议、音乐播放器这些**对延迟敏感**的软件 —— 翻译器把 CPU 吃满，
/// 它们就会卡。
///
/// 所以档位不只由用户拍脑袋选，还给出**基于实际 CPU / 内存占用率的建议**：
/// 点「检测当前占用」，系统按真实负载算出推荐档位，用户一键采用。
class _ResourceTierTile extends StatelessWidget {
  final TranslatorState state;
  const _ResourceTierTile({required this.state});

  static const _fallbackTiers = <ResourceTierInfo>[
    ResourceTierInfo('light', '轻量', '同时运行其他重负载（游戏、大型编译）时选它', 1, 1),
    ResourceTierInfo('balanced', '均衡', 'CPU 或内存比较紧张时选它', 2, 2),
    // 线程数与 Rust 侧 `ResourceTier::asr_threads()/mt_threads()` 保持一致。
    // 注意：默认档已不是 Full —— 2026-10-25 实测（bench_run.py 四组对照）
    // 证明质量与线程数无关、4 线程速度余量已够，默认改为 4/4。
    ResourceTierInfo('full', '全力', '机器空闲、要极限速度时选它（质量并不更高）', 8, 6),
  ];

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final tiers = state.tierInfos.isNotEmpty ? state.tierInfos : _fallbackTiers;
    final load = state.systemLoad;
    final selected = tiers.firstWhere(
      (t) => t.key == state.resourceTier,
      orElse: () => tiers.length > 1 ? tiers[1] : tiers.first,
    );
    final recommended = load?.recommended;
    final isRecommended = recommended != null && recommended == selected.key;

    return Card(
      margin: EdgeInsets.zero,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('资源档位', style: theme.textTheme.titleMedium),
            const SizedBox(height: 4),
            Text(
              '线程数只影响速度余量，不影响识别与翻译质量（实测：各档质量'
              '完全一致）。默认 4：实时会议跟得上、又不会和会议软件抢 CPU'
              '造成卡顿。只有快语速连讲仍跟不上时才需要调高。',
              style: theme.textTheme.bodySmall,
            ),
            const SizedBox(height: 12),
            // ── 线程数 Slider（2026-10-25 用户要求：像上下文那样的拖动选择）──
            //
            // 识别线程 1-8 直接拖动；翻译线程随之联动 min(识别, 6)。
            // 比原来的三档单选更直观，也绕开了曾失效的 resource_tier 通路。
            Row(
              children: [
                Text('识别线程', style: theme.textTheme.bodyMedium),
                const Spacer(),
                Text('${state.asrThreads}', style: theme.textTheme.titleMedium),
              ],
            ),
            Slider(
              value: state.asrThreads.clamp(1, 8).toDouble(),
              min: 1,
              max: 8,
              divisions: 7,
              label: '识别 ${state.asrThreads} 线程',
              // 2026-10-25：实时生效 —— 转写中拖动也会通知引擎，
              // 线程数在下一句边界热切换，无需重启。
              onChanged: (v) => state.updateLiveThreads(v.round()),
            ),
            Padding(
              padding: const EdgeInsets.only(left: 4, bottom: 4),
              child: Text(
                '转写中拖动即时生效：线程数在下一句边界热切换'
                '（切换后首句段首质量可能短暂下降）；'
                '其他参数下一句立即生效。',
                style: theme.textTheme.bodySmall
                    ?.copyWith(color: theme.hintColor, fontSize: 10),
              ),
            ),
            const Divider(height: 20),
            // 断句灵敏度（VAD hangover）—— 实时生效
            Row(
              children: [
                Text('断句灵敏度', style: theme.textTheme.bodyMedium),
                const Spacer(),
                Text('${state.hangoverMs} ms',
                    style: theme.textTheme.titleMedium),
              ],
            ),
            Slider(
              value: state.hangoverMs.clamp(200, 2000).toDouble(),
              min: 200,
              max: 2000,
              divisions: 18,
              label: '${state.hangoverMs} ms',
              onChanged: (v) => state.updateLiveHangover(v.round()),
            ),
            Padding(
              padding: const EdgeInsets.only(left: 4, bottom: 4),
              child: Text(
                '句尾静音多久后断句。越小越跟手，越大越不易碎句；'
                '连续演讲建议 600 ms 以上。转写中调整下一句立即生效。',
                style: theme.textTheme.bodySmall
                    ?.copyWith(color: theme.hintColor, fontSize: 10),
              ),
            ),
            const Divider(height: 20),
            // 离线精修开关（两遍解码第二遍）—— 实时生效
            SwitchListTile(
              dense: true,
              secondary: const Icon(Icons.auto_fix_high),
              title: const Text('离线精修（第二遍解码）'),
              subtitle: const Text(
                '句子成行后用 SenseVoice 重识别修正，质量更高、句末略慢。'
                '缺失模型时自动退回单遍。转写中切换下一句立即生效。',
                style: TextStyle(fontSize: 11),
              ),
              value: state.refineEnabled,
              onChanged: (v) => state.setRefineEnabled(v),
            ),
            Row(
              children: [
                Text('翻译线程', style: theme.textTheme.bodyMedium),
                const Spacer(),
                Text('${state.mtThreads}', style: theme.textTheme.titleMedium),
              ],
            ),
            Padding(
              padding: const EdgeInsets.only(top: 2, bottom: 4),
              child: Text(
                _tierAdvice(state.asrThreads),
                style: theme.textTheme.bodySmall,
              ),
            ),
            const Divider(height: 20),
            // 负载探测
            Row(
              children: [
                Expanded(
                  child: Text(
                    load == null
                        ? '尚未检测当前占用'
                        : 'CPU ${load.cpu.toStringAsFixed(0)}%'
                            ' · 内存 ${load.mem.toStringAsFixed(0)}%',
                    style: theme.textTheme.bodyMedium,
                  ),
                ),
                if (state.probingLoad)
                  const SizedBox(
                    width: 16,
                    height: 16,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  )
                else
                  TextButton.icon(
                    onPressed: state.probeSystemLoad,
                    icon: const Icon(Icons.speed, size: 18),
                    label: const Text('检测当前占用'),
                  ),
              ],
            ),
            if (load != null && load.advice.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Text(
                  load.advice,
                  style: theme.textTheme.bodySmall,
                ),
              ),
            if (load != null && load.top.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: 6),
                child: Text(
                  '占用最多的进程：'
                  '${load.top.take(3).map((p) => '${p.name} ${p.cpu.toStringAsFixed(0)}%').join('、')}',
                  style: theme.textTheme.bodySmall?.copyWith(
                    color: theme.colorScheme.outline,
                  ),
                ),
              ),
            if (recommended != null && !isRecommended)
              Padding(
                padding: const EdgeInsets.only(top: 8),
                child: Align(
                  alignment: Alignment.centerLeft,
                  child: TextButton.icon(
                    onPressed: state.adoptRecommendedTier,
                    icon: const Icon(Icons.auto_awesome, size: 18),
                    // 优先显示线程数推荐（实测成本模型算出来的，比三档更准）
                    label: state.recommendedThreads > 0
                        ? Text('采用建议（识别 ${state.recommendedThreads} 线程）')
                        : Text(
                            '采用建议（${tiers.firstWhere((t) => t.key == recommended, orElse: () => selected).name}）'),
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

/// 能力卡片：清晰展示可用/降级/不可用
class _CapabilityTile extends StatelessWidget {
  final TranslatorState state;
  const _CapabilityTile({required this.state});

  @override
  Widget build(BuildContext context) {
    final cap = state.capability;
    final (icon, color) = cap.systemAudio
        ? (Icons.check_circle, Colors.green)
        : cap.available
            ? (Icons.warning, Colors.orange)
            : (Icons.error, Colors.red);

    return Card(
      margin: EdgeInsets.zero,
      child: ListTile(
        leading: Icon(icon, color: color, size: 28),
        title: Text(
          cap.systemAudio
              ? '系统内放采集可用'
              : cap.available
                  ? '已降级为麦克风'
                  : '采集不可用',
        ),
        subtitle: Text(cap.describe(), style: const TextStyle(fontSize: 11)),
        isThreeLine: true,
      ),
    );
  }
}

/// 卓易通专属引导
class _ZhuoYiTongGuide extends StatelessWidget {
  @override
  Widget build(BuildContext context) {
    return Card(
      margin: EdgeInsets.zero,
      color: Colors.blue.shade50,
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(Icons.info_outline, size: 16, color: Colors.blue.shade700),
                const SizedBox(width: 6),
                Text('卓易通使用须知',
                    style: TextStyle(
                        fontWeight: FontWeight.bold,
                        color: Colors.blue.shade700)),
              ],
            ),
            const SizedBox(height: 8),
            ...const [
              '1. 权限需在卓易通的「应用信息」页逐项手动授予',
              '2. 容器无法读取手机存储，模型与词库须经「文件互传」导入',
              '3. 建议将卓易通加入电池优化白名单，避免容器休眠导致断流',
              '4. 容器内系统音频捕获能力受限，通常需搭配虚拟声卡使用',
            ].map((t) => Padding(
                  padding: const EdgeInsets.only(bottom: 4),
                  child: Text(t, style: const TextStyle(fontSize: 11)),
                )),
          ],
        ),
      ),
    );
  }
}

class _Section extends StatelessWidget {
  final String title;
  final List<Widget> children;
  const _Section({required this.title, required this.children});

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Padding(
          padding: const EdgeInsets.only(bottom: 8),
          child: Text(title,
              style: Theme.of(context)
                  .textTheme
                  .titleSmall
                  ?.copyWith(color: Theme.of(context).hintColor)),
        ),
        Card(
          margin: EdgeInsets.zero,
          child: Column(children: children),
        ),
      ],
    );
  }
}
