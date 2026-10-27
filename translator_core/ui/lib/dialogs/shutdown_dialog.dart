import 'package:flutter/material.dart';

/// 退出遮罩（纯展示组件）。
///
/// 职责单一：渲染「即将退出，正在保存」的全窗遮罩（纯静态，无进度条 ——
/// 退出全程只有几百毫秒，进度条来不及表达任何信息，反而增加视觉噪音）。
/// 清理时序（关字幕 → 停转写 → 销毁窗口 → 退出）由调用方
/// （`main.dart` 的 `tray.onQuit`）按序 await 完成 —— 本组件不持有状态、
/// 不在 build 中触发副作用，避免遮罩卡死或程序不退出。
class ShutdownDialog extends StatelessWidget {
  /// 遮罩上显示的提示文案
  final String message;

  const ShutdownDialog({
    super.key,
    this.message = '即将退出，正在保存，请稍候',
  });

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);

    return Container(
      // 全窗半透明遮罩
      color: Colors.black.withValues(alpha: 0.6),
      child: Center(
        child: Container(
          padding: const EdgeInsets.all(24),
          decoration: BoxDecoration(
            color: Colors.white.withValues(alpha: 0.95),
            borderRadius: BorderRadius.circular(16),
            boxShadow: [
              BoxShadow(
                color: Colors.black.withValues(alpha: 0.3),
                blurRadius: 20,
                spreadRadius: 4,
              ),
            ],
          ),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(Icons.save, size: 48, color: theme.primaryColor),
              const SizedBox(height: 16),
              Text(
                '即将退出',
                style: theme.textTheme.headlineSmall?.copyWith(
                  fontWeight: FontWeight.bold,
                ),
              ),
              const SizedBox(height: 8),
              Text(
                message,
                textAlign: TextAlign.center,
                style: theme.textTheme.bodyMedium?.copyWith(
                  color: Colors.black54,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
