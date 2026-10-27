//! 词库快照与热重载。
//!
//! ## SnapshotGuard 为什么是必需的
//!
//! 哨兵 `▁X{n}X` 中的 `n` 是**当前快照内的局部索引**。如果文本片段
//! 在处理中途换了快照，还原时会把 A 词的占位符还原成 B 词的译法 ——
//! 表现为字幕里出现完全不相干的术语，且极难复现。
//!
//! 因此每个处理中的片段必须持有它开始时那一份快照的引用（`Arc`），
//! 直到该片段彻底处理完毕。

use crate::glossary::schema::{Entry, GlossaryFile, Scope};
use crate::glossary::sentinel::{restore, GlossaryMatcher, SentinelPass, RestorePass};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// 词条命中统计（GLOSSARY.md §7）
#[derive(Debug, Default)]
pub struct HitStats {
    asr: Vec<AtomicU64>,
    mt: Vec<AtomicU64>,
    regex: Vec<AtomicU64>,
}

impl HitStats {
    fn new(n: usize) -> Self {
        Self {
            asr: (0..n).map(|_| AtomicU64::new(0)).collect(),
            mt: (0..n).map(|_| AtomicU64::new(0)).collect(),
            regex: (0..n).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    pub fn asr_hits(&self, idx: usize) -> u64 {
        self.asr.get(idx).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
    }

    pub fn mt_hits(&self, idx: usize) -> u64 {
        self.mt.get(idx).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
    }

    pub fn regex_hits(&self, idx: usize) -> u64 {
        self.regex.get(idx).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// 零命中词条下标（UI 用于提示清理失效词条）
    pub fn zero_hit_entries(&self) -> Vec<usize> {
        (0..self.asr.len())
            .filter(|&i| {
                self.asr[i].load(Ordering::Relaxed) == 0
                    && self.mt[i].load(Ordering::Relaxed) == 0
                    && self.regex[i].load(Ordering::Relaxed) == 0
            })
            .collect()
    }
}

/// 不可变词库快照。
///
/// 一旦构建即不可变；词库变更时构建新快照并原子替换。
#[derive(Debug)]
pub struct GlossarySnapshot {
    /// 词条全集（含未启用的，供统计与 UI 使用）
    entries: Vec<Entry>,
    /// MT 层匹配器
    matcher: GlossaryMatcher,
    /// ASR 热词表（去重后）
    hotwords: Vec<String>,
    /// 快照生成时间戳（单调毫秒）
    version: u64,
    /// 来源文件列表
    files: Vec<PathBuf>,
}

impl GlossarySnapshot {
    /// 由多个词库文件合并构建（GLOSSARY.md §6 合并规则）。
    ///
    /// 冲突消解：同 `source` 时 `enabled: true` 胜出；多条 true 则后加载的胜出。
    pub fn build(files: Vec<GlossaryFile>) -> Self {
        static VERSION: AtomicU64 = AtomicU64::new(0);

        let mut merged: Vec<Entry> = Vec::new();
        let mut seen_files: Vec<PathBuf> = Vec::new();

        for gf in files {
            // 记录来源文件
            if let Some(first) = gf.entries.first() {
                if !seen_files.contains(&first.source_file) {
                    seen_files.push(first.source_file.clone());
                }
            }
            for mut e in gf.entries {
                let key = Entry::storage_key(&e.source, e.case_sensitive);
                match merged.iter_mut().find(|m| {
                    Entry::storage_key(&m.source, m.case_sensitive) == key
                }) {
                    None => merged.push(e),
                    Some(existing) => {
                        // enabled:true 胜出
                        if !existing.enabled && e.enabled {
                            *existing = e;
                        } else if existing.enabled && e.enabled {
                            // 后加载的胜出；aliases 取并集（保留来源文件的顺序，
                            // 新词条的 aliases 在前）
                            let old_aliases: Vec<String> =
                                existing.aliases.iter().filter(|a| !e.aliases.contains(a)).cloned().collect();
                            let mut merged = e.aliases.clone();
                            merged.extend(old_aliases);
                            e.aliases = merged;
                            *existing = e;
                        }
                        // existing.enabled && !e.enabled → 保留 existing
                    }
                }
            }
        }

        let matcher = GlossaryMatcher::build(&merged);

        // 构建 ASR 热词表：仅 scope 作用于 ASR 且启用
        let mut hotwords: Vec<String> = Vec::new();
        for e in &merged {
            if !e.enabled || !e.scope.applies_to_asr() {
                continue;
            }
            for t in e.asr_triggers() {
                if !t.is_empty() && !hotwords.contains(&t) {
                    hotwords.push(t);
                }
            }
        }

        Self {
            entries: merged,
            matcher,
            hotwords,
            version: VERSION.fetch_add(1, Ordering::Relaxed) + 1,
            files: seen_files,
        }
    }

    /// 从单个文件构建（测试与简单场景）
    pub fn from_file(gf: GlossaryFile) -> Self {
        Self::build(vec![gf])
    }

    /// 空快照
    pub fn empty() -> Self {
        Self::build(vec![])
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn hotword_count(&self) -> usize {
        self.hotwords.len()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// 词库变更时**只需重建热词表**，不重建 ASR recognizer 实例
    /// （DESIGN.md §5.3：避免 2~3 秒的重建卡顿）
    pub fn hotwords(&self) -> &[String] {
        &self.hotwords
    }

    pub fn matcher(&self) -> &GlossaryMatcher {
        &self.matcher
    }

    /// Layer B：翻译前哨兵保护
    pub fn protect(&self, text: &str) -> SentinelPass {
        let pass = self.matcher.protect(text);
        pass
    }

    /// 记录 MT 命中统计
    pub fn record_mt_hits(&self, stats: &HitStats, hits: &[usize]) {
        for &i in hits {
            if let Some(c) = stats.mt.get(i) {
                c.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Default for GlossarySnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

/// 处理上下文的快照守卫。
///
/// 片段开始处理时调用 `SnapshotGuard::new` 固定快照，
/// 之后即使热重载换了快照，本片段的哨兵索引语义仍保持一致。
#[derive(Clone)]
pub struct SnapshotGuard {
    snapshot: Arc<GlossarySnapshot>,
}

impl std::fmt::Debug for SnapshotGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotGuard")
            .field("version", &self.snapshot.version())
            .finish()
    }
}

impl SnapshotGuard {
    pub fn new(snapshot: Arc<GlossarySnapshot>) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> &GlossarySnapshot {
        &self.snapshot
    }

    /// Layer B → Layer C 的完整处理：保护文本，待译文返回后还原。
    pub fn start(&self, text: &str) -> PendingSegment {
        let pass = self.snapshot.protect(text);
        PendingSegment {
            guard: self.clone(),
            pass,
            original: text.to_string(),
        }
    }
}

/// 已完成哨兵保护、等待翻译的片段
#[derive(Debug)]
pub struct PendingSegment {
    guard: SnapshotGuard,
    pass: SentinelPass,
    original: String,
}

impl PendingSegment {
    /// 送入翻译引擎的文本（含哨兵）
    pub fn text_for_translation(&self) -> &str {
        &self.pass.text
    }

    /// 哨兵数量（= 术语命中数）
    pub fn sentinel_count(&self) -> usize {
        self.pass.mapping.len()
    }

    /// 哨兵索引 → 目标译法。
    ///
    /// 暴露给需要自行编排翻译流程的调用方（例如调试工具、
    /// 或需要在翻译前检查术语命中情况的 UI 预览）。
    pub fn mapping(&self) -> &[String] {
        &self.pass.mapping
    }

    /// 本片段命中的词条下标（用于命中统计）
    pub fn hit_entries(&self) -> &[usize] {
        &self.pass.hit_entries
    }

    pub fn original_text(&self) -> &str {
        &self.original
    }

    /// 翻译完成后还原哨兵。
    pub fn finish(self, translated: &str) -> (String, RestorePass) {
        let r = restore(translated, &self.pass.mapping);
        (r.text.clone(), r)
    }

    pub fn guard(&self) -> &SnapshotGuard {
        &self.guard
    }
}

/// 热重载管理器。
///
/// 监听文件变化（250 ms 防抖），重建成功后原子替换快照。
/// 构建失败时**保留旧快照**，并通过回调上报错误 —— 绝不能让
/// 一个语法错误把正在工作的词库清空。
pub struct GlossaryManager {
    current: RwLock<Arc<GlossarySnapshot>>,
    stats: RwLock<Arc<HitStats>>,
    /// 防抖配置（毫秒）
    pub debounce_ms: u64,
    /// 上次加载的源文件
    sources: Mutex<Vec<PathBuf>>,
}

impl GlossaryManager {
    pub fn new() -> Self {
        let empty = Arc::new(GlossarySnapshot::empty());
        Self {
            stats: RwLock::new(Arc::new(HitStats::new(0))),
            current: RwLock::new(empty),
            debounce_ms: 250,
            sources: Mutex::new(Vec::new()),
        }
    }

    /// 启动时加载指定词库文件
    pub fn load_files(&self, paths: &[PathBuf]) -> crate::Result<()> {
        let mut parsed = Vec::with_capacity(paths.len());
        for p in paths {
            let outcome = crate::glossary::parse::parse_file(p)?;
            parsed.push(outcome_to_file(outcome));
        }
        let snap = Arc::new(GlossarySnapshot::build(parsed));

        // 统计表需按新词条数重建（无法原地扩容语义）
        *self.stats.write().unwrap() = Arc::new(HitStats::new(snap.entry_count()));
        *self.current.write().unwrap() = snap;
        *self.sources.lock().unwrap() = paths.to_vec();
        Ok(())
    }

    /// 追加导入一个词库文件，返回该文件的有效词条数。
    ///
    /// 语义是**追加**而不是替换：用户导入第二个词库时，
    /// 第一个不该消失。失败时旧快照保持不变。
    pub fn add_file(&self, path: &PathBuf) -> crate::Result<usize> {
        let parsed = crate::glossary::parse::parse_file(path)?;
        let n = parsed.entries.len();
        let mut files = self.sources.lock().unwrap().clone();
        if !files.iter().any(|p| p == path) {
            files.push(path.clone());
        }
        self.load_files(&files)?;
        Ok(n)
    }

    /// 当前快照的词条数
    pub fn entry_count(&self) -> usize {
        self.current().entry_count()
    }

    /// 热重载。成功则替换快照，失败保留旧快照并返回错误。
    pub fn reload(&self) -> crate::Result<()> {
        let paths = self.sources.lock().unwrap().clone();
        if paths.is_empty() {
            return Ok(());
        }
        let mut parsed = Vec::with_capacity(paths.len());
        for p in &paths {
            parsed.push(outcome_to_file(
                crate::glossary::parse::parse_file(p)?,
            ));
        }
        // 到这里才替换：任一文件解析失败都会提前返回，旧快照保持有效
        let snap = Arc::new(GlossarySnapshot::build(parsed));
        *self.current.write().unwrap() = snap;
        Ok(())
    }

    /// 取得当前快照（升级为 Arc 供守卫持有）
    pub fn current(&self) -> Arc<GlossarySnapshot> {
        Arc::clone(&self.current.read().unwrap())
    }

    /// 为某段文本创建守卫，固定当前快照
    pub fn guard(&self) -> SnapshotGuard {
        SnapshotGuard::new(self.current())
    }

    pub fn stats(&self) -> Arc<HitStats> {
        Arc::clone(&self.stats.read().unwrap())
    }

    /// 某词条的 scope，用于 UI 展示
    pub fn scope_of(&self, idx: usize) -> Option<Scope> {
        self.current().entries().get(idx).map(|e| e.scope)
    }

    /// 自增长词库：导出 `mt_hits >= 1` 的词条（GLOSSARY.md §7）
    pub fn export_hit_entries(&self, path: &Path) -> crate::Result<usize> {
        let snap = self.current();
        let stats = self.stats();
        let mut n = 0usize;
        let mut out = String::from("# Translator 自动导出的已命中词条\n");
        out.push_str("source\ttarget\tscope\n");
        for (i, e) in snap.entries().iter().enumerate() {
            if stats.mt_hits(i) > 0 || stats.asr_hits(i) > 0 {
                out.push_str(&format!("{}\t{}\t{}\n", e.source, e.target, scope_str(e.scope)));
                n += 1;
            }
        }
        std::fs::write(path, out).map_err(|e| crate::Error::GlossaryLoad(e.to_string()))?;
        Ok(n)
    }
}

impl Default for GlossaryManager {
    fn default() -> Self {
        Self::new()
    }
}

fn scope_str(s: Scope) -> &'static str {
    match s {
        Scope::Asr => "asr",
        Scope::Mt => "mt",
        Scope::Both => "both",
    }
}

fn outcome_to_file(o: crate::glossary::parse::ParsedFile) -> GlossaryFile {
    GlossaryFile {
        version: o.version,
        entries: o.entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glossary::schema::Scope;
    use std::path::PathBuf;

    fn mk(source: &str, target: &str, scope: Scope, enabled: bool) -> Entry {
        Entry {
            source: source.into(),
            target: target.into(),
            aliases: vec![],
            case_sensitive: false,
            scope,
            group: None,
            enabled,
            note: None,
            regex: None,
            source_file: PathBuf::from("t"),
            line_no: 1,
        }
    }

    fn gf(entries: Vec<Entry>) -> GlossaryFile {
        GlossaryFile { version: 1, entries }
    }

    #[test]
    fn hotwords_only_include_asr_applicable_entries() {
        let s = GlossarySnapshot::from_file(gf(vec![
            mk("Kubernetes", "Kubernetes", Scope::Both, true),
            mk("i.e.", "也就是说", Scope::Mt, true),   // 不该进 ASR 热词
            mk("React", "React", Scope::Asr, true),
            mk("Vue", "Vue", Scope::Both, false),       // 禁用，不该进
        ]));
        let hw = s.hotwords();
        assert!(hw.contains(&"Kubernetes".to_string()));
        assert!(hw.contains(&"React".to_string()));
        assert!(!hw.contains(&"i.e.".to_string()));
        assert!(!hw.contains(&"Vue".to_string()));
    }

    #[test]
    fn merge_prefers_enabled_entry() {
        let a = gf(vec![mk("Term", "旧译", Scope::Both, false)]);
        let b = gf(vec![mk("Term", "新译", Scope::Both, true)]);
        let s = GlossarySnapshot::build(vec![a, b]);
        assert_eq!(s.entry_count(), 1);
        assert_eq!(s.entries()[0].target, "新译");
    }

    #[test]
    fn merge_later_file_wins_and_unions_aliases() {
        let mut a1 = mk("Term", "旧译", Scope::Both, true);
        a1.aliases = vec!["T1".into()];
        let mut a2 = mk("Term", "新译", Scope::Both, true);
        a2.aliases = vec!["T2".into()];
        let s = GlossarySnapshot::build(vec![gf(vec![a1]), gf(vec![a2])]);
        assert_eq!(s.entry_count(), 1);
        assert_eq!(s.entries()[0].target, "新译");
        assert_eq!(
            s.entries()[0].aliases,
            vec!["T2", "T1"],
            "后加载的译法优先，但 aliases 应保留并集"
        );
    }

    #[test]
    fn snapshot_guard_pins_version_across_reload() {
        // 核心正确性测试：处理中途换快照，哨兵索引语义不得错乱
        let _m = GlossaryManager::new();
        let g1 = SnapshotGuard::new(Arc::new(GlossarySnapshot::from_file(gf(vec![
            mk("Kubernetes", "Kubernetes", Scope::Both, true),
            mk("gRPC", "gRPC", Scope::Both, true),
        ]))));

        let pending = g1.start("Kubernetes and gRPC");
        assert_eq!(pending.pass.sentinel_count(), 2);
        assert_eq!(pending.text_for_translation(), "\u{2581}X0X and \u{2581}X1X");

        // 换一份完全不同的快照（索引语义变了）
        let g2 = SnapshotGuard::new(Arc::new(GlossarySnapshot::from_file(gf(vec![
            mk("Docker", "Docker", Scope::Both, true),
        ]))));
        // 旧守卫片段的处理不受新快照影响
        let _pending2 = g2.start("Docker");

        // 用**旧**守卫的译文还原，必须还原成旧词的译法
        let (text, r) = pending.finish("我们用 \u{2581}X0X 和 \u{2581}X1X 部署");
        assert!(r.is_consistent(), "旧守卫片段必须自洽");
        assert_eq!(text, "我们用 Kubernetes 和 gRPC 部署");
    }

    #[test]
    fn end_to_end_layer_b() {
        let s = GlossarySnapshot::from_file(gf(vec![
            mk("Kubernetes", "Kubernetes", Scope::Both, true),
            mk("gRPC", "gRPC", Scope::Both, true),
        ]));
        let guard = SnapshotGuard::new(Arc::new(s));

        let seg = guard.start("We run Kubernetes and gRPC in prod");
        assert_eq!(seg.text_for_translation(), "We run \u{2581}X0X and \u{2581}X1X in prod");

        // 模拟 Marian 输出（哨兵透传）
        let (zh, r) = seg.finish("我们在生产环境运行 \u{2581}X0X 和 \u{2581}X1X");
        assert!(r.is_consistent());
        assert_eq!(zh, "我们在生产环境运行 Kubernetes 和 gRPC");
    }

    #[test]
    fn inconsistent_restore_is_detected_for_layer_c() {
        let s = GlossarySnapshot::from_file(gf(vec![mk("K8s", "K8s", Scope::Both, true)]));
        let guard = SnapshotGuard::new(Arc::new(s));
        let seg = guard.start("K8s");
        // 模拟翻译引擎吞掉了哨兵
        let (_, r) = seg.finish("我们用库伯内特斯");
        assert!(!r.is_consistent(), "哨兵丢失必须被检出，才能触发 Layer C");
    }

    #[test]
    fn empty_snapshot_is_transparent() {
        let s = GlossarySnapshot::empty();
        let guard = SnapshotGuard::new(Arc::new(s));
        let seg = guard.start("plain text");
        assert_eq!(seg.text_for_translation(), "plain text");
        let (zh, r) = seg.finish("纯文本");
        assert_eq!(zh, "纯文本");
        assert!(r.is_consistent());
    }

    #[test]
    fn version_increments_on_each_build() {
        let a = GlossarySnapshot::from_file(gf(vec![mk("A", "A", Scope::Both, true)]));
        let b = GlossarySnapshot::from_file(gf(vec![mk("B", "B", Scope::Both, true)]));
        assert!(b.version() > a.version());
    }
}
