//! 不依赖原生库的参考翻译实现。
//!
//! 用途：
//! 1. CT2 原生库未编译好时跑通整条链路
//! 2. **验证哨兵透传假设** —— 本项目的核心机制依赖"Marian 会原样透传
//!    `▁X{n}X`"。Mock 按此行为实现；若真实模型不透传，测试会暴露。
//!
//! 它不做真实翻译，而是把英文映射到固定中文，保证输出可断言。

use super::{MtBackend, MtConfig, QualityTier, Translation};
use crate::error::{Error, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct MockStats {
    pub translate_calls: AtomicU64,
    pub chars_in: AtomicU64,
}

pub struct MockTranslator {
    stats: Arc<MockStats>,
    tier: QualityTier,
    /// 固定延迟，用于验证背压行为
    simulated_latency_ms: u64,
    /// 词条表：英文小写 → 中文
    lexicon: Vec<(String, String)>,
}

impl MockTranslator {
    pub fn new(tier: QualityTier) -> Self {
        Self {
            stats: Arc::new(MockStats::default()),
            tier,
            simulated_latency_ms: 0,
            lexicon: default_lexicon(),
        }
    }

    pub fn with_latency(tier: QualityTier, ms: u64) -> Self {
        let mut t = Self::new(tier);
        t.simulated_latency_ms = ms;
        t
    }

    pub fn stats(&self) -> Arc<MockStats> {
        Arc::clone(&self.stats)
    }

    /// 伪翻译：逐词查表，查不到则原样保留。
    /// 哨兵 `▁X{n}X` 原样透传 —— 这正是 Marian 的预期行为。
    fn pseudo_translate(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut token = String::new();

        let flush = |token: &mut String, out: &mut String, lex: &Vec<(String, String)>| {
            if token.is_empty() {
                return;
            }
            // 哨兵原样透传
            if token.starts_with("\u{2581}X") && token.ends_with('X') {
                out.push_str(token);
            } else {
                let key = token.to_lowercase();
                // 必须取**最长**匹配：词典里同时有 deploy / deployed 时，
                // 按插入顺序 find 会先命中 deploy，把 deployed 误译成"部署"
                match lex
                    .iter()
                    .filter(|(k, _)| *k == key)
                    .max_by_key(|(k, _)| k.len())
                {
                    Some((_, zh)) => out.push_str(zh),
                    None => out.push_str(token),
                }
            }
            token.clear();
        };
        for c in text.chars() {
            if c.is_whitespace() {
                flush(&mut token, &mut out, &self.lexicon);
                out.push(' ');
            } else {
                token.push(c);
            }
        }
        flush(&mut token, &mut out, &self.lexicon);
        out.trim().to_string()
    }
}

impl MtBackend for MockTranslator {
    fn translate(&self, protected_text: &str) -> Result<Translation> {
        self.stats
            .translate_calls
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .chars_in
            .fetch_add(protected_text.chars().count() as u64, Ordering::Relaxed);

        if self.simulated_latency_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.simulated_latency_ms));
        }

        Ok(Translation {
            text: self.pseudo_translate(protected_text),
            consistent: true,
            elapsed_ms: self.simulated_latency_ms,
        })
    }

    fn translate_batch(&self, texts: &[String]) -> Result<Vec<Translation>> {
        texts.iter().map(|t| self.translate(t)).collect()
    }

    fn name(&self) -> &'static str {
        "mock (测试用，非真实翻译)"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn tier(&self) -> QualityTier {
        self.tier
    }
}

/// 从 MT 配置构造（便于测试替换真实后端）
pub fn from_config(cfg: &MtConfig) -> Result<Arc<dyn MtBackend>> {
    if !cfg.model_dir.exists() {
        return Err(Error::ModelMissing(format!(
            "翻译模型目录不存在: {}",
            cfg.model_dir.display()
        )));
    }
    Ok(Arc::new(MockTranslator::new(cfg.tier)))
}

fn default_lexicon() -> Vec<(String, String)> {
    vec![
        ("we".into(), "我们".into()),
        ("deploy".into(), "部署".into()),
        ("deployed".into(), "部署了".into()),
        ("today".into(), "今天".into()),
        ("hello".into(), "你好".into()),
        ("world".into(), "世界".into()),
        ("is".into(), "是".into()),
        ("the".into(), "这个".into()),
        ("in".into(), "在".into()),
        ("production".into(), "生产环境".into()),
        ("and".into(), "和".into()),
        ("a".into(), "一个".into()),
        ("test".into(), "测试".into()),
        ("system".into(), "系统".into()),
        ("uses".into(), "使用".into()),
        ("for".into(), "用于".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k, v))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_known_words() {
        let t = MockTranslator::new(QualityTier::Fast);
        let r = t.translate("hello world").unwrap();
        assert_eq!(r.text, "你好 世界");
        assert!(r.consistent);
    }

    #[test]
    fn inflected_forms_map_to_distinct_translations() {
        let t = MockTranslator::new(QualityTier::Fast);
        assert_eq!(t.translate("deploy").unwrap().text, "部署");
        assert_eq!(t.translate("deployed").unwrap().text, "部署了");
    }

    #[test]
    fn unknown_words_pass_through() {
        let t = MockTranslator::new(QualityTier::Fast);
        let r = t.translate("quantum flux capacitor").unwrap();
        assert_eq!(r.text, "quantum flux capacitor");
    }

    #[test]
    fn sentinels_pass_through_untouched() {
        // 核心假设：Marian 不会翻译 ▁X{n}X
        let t = MockTranslator::new(QualityTier::Fast);
        let r = t.translate("we deploy \u{2581}X0X today").unwrap();
        assert!(
            r.text.contains("\u{2581}X0X"),
            "哨兵必须原样透传，实际: {}",
            r.text
        );
    }

    #[test]
    fn multiple_sentinels_all_pass_through() {
        let t = MockTranslator::new(QualityTier::Fast);
        let r = t
            .translate("we use \u{2581}X0X and \u{2581}X1X today")
            .unwrap();
        assert!(r.text.contains("\u{2581}X0X"));
        assert!(r.text.contains("\u{2581}X1X"));
    }

    #[test]
    fn empty_input_returns_empty() {
        let t = MockTranslator::new(QualityTier::Fast);
        assert_eq!(t.translate("").unwrap().text, "");
    }

    #[test]
    fn batch_translation() {
        let t = MockTranslator::new(QualityTier::Fast);
        let texts = vec!["hello world".to_string(), "we deploy today".to_string()];
        let r = t.translate_batch(&texts).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].text, "你好 世界");
    }

    #[test]
    fn stats_track_calls() {
        let t = MockTranslator::new(QualityTier::Fast);
        let stats = t.stats();
        t.translate("hello").unwrap();
        t.translate("world").unwrap();
        assert_eq!(stats.translate_calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn backend_is_send_and_sync() {
        fn assert_ss<T: Send + Sync>() {}
        assert_ss::<MockTranslator>();
    }

    #[test]
    fn end_to_end_with_glossary_sentinel() {
        // 完整闭环：词库保护 → 伪翻译 → 哨兵还原
        use crate::glossary::schema::{Entry, Scope};
        use crate::glossary::{GlossaryFile, GlossarySnapshot, SnapshotGuard};
        use std::path::PathBuf;
        use std::sync::Arc as StdArc;

        let snapshot = GlossarySnapshot::from_file(GlossaryFile {
            version: 1,
            entries: vec![Entry {
                source: "Kubernetes".into(),
                target: "Kubernetes".into(),
                aliases: vec![],
                case_sensitive: false,
                scope: Scope::Both,
                group: None,
                enabled: true,
                note: None,
                regex: None,
                source_file: PathBuf::from("t"),
                line_no: 1,
            }],
        });
        let guard = SnapshotGuard::new(StdArc::new(snapshot));
        let pending = guard.start("we deploy Kubernetes today");
        assert_eq!(pending.text_for_translation(), "we deploy \u{2581}X0X today");

        let t = MockTranslator::new(QualityTier::Fast);
        let out = t.translate(pending.text_for_translation()).unwrap();
        let (zh, restore) = pending.finish(&out.text);

        assert!(restore.is_consistent());
        // 源文为 "deploy"（非 deployed），故译文是"部署"
        assert_eq!(zh, "我们 部署 Kubernetes 今天");
    }

    #[test]
    fn missing_model_dir_errors() {
        let cfg = MtConfig::new("nonexistent/model/path", 2);
        assert!(from_config(&cfg).is_err());
    }
}
