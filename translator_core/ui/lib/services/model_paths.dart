import '../ffi/bridge.dart';
import '../state/translator_state.dart';

/// 解析当前生效的模型路径（模型页 / 词库页共用）。
///
/// ## 为什么不能只靠 `bridge.discoverModels()`
///
/// Rust 侧的 `discover_model_layout` 只探三处：
/// - `<exe 目录>/models`
/// - `<exe 目录>/../models`
/// - `<cwd>/models`
///
/// 桌面端 exe 与 models 同级（或隔一级），探得到。
/// 但 **Android 上 `current_exe` 是 .so 所在目录**（`app/lib/arm64`），
/// 那三处全都不存在 → 必然返回空字符串 → 模型页所有卡片显示「缺失」，
/// 路径与占用一片空白。
///
/// （2026-10-28 用户反馈「模型启动页面说已经正常使用了，但模型页面
/// 还是显示里面没东西」—— 就是这个坑：引擎用的是 Dart 侧探测到的
/// 路径，能正常加载；而模型页用的是 Rust 侧探测，两条路径不通。）
///
/// ## Android 的真实落点
///
/// `<internalFilesDir>/models/{asr,mt,offline,glossary}`
/// —— 首启由 Kotlin 的 `ModelExtractor` 从 assets 解压。
/// 这个探测 `TranslatorState` 已经做过了（`_probeAndroidModels`，
/// 内部私有目录优先、外部私有目录兜底），结果放在 `state.models` 里
/// —— 直接复用，不要再探一遍。
ModelPaths resolveModelPaths(TranslatorState state) {
  final m = state.models;
  if (m.asr.isNotEmpty || m.mt.isNotEmpty) return m;
  // 桌面端（或 bootstrap 尚未完成）→ 回退到 Rust 侧探测
  return state.bridge.discoverModels();
}
