//! 模型目录规格与校验。
//!
//! ## 为什么独立成模块，而不是放在 sherpa / bergamot 里
//!
//! `asr/sherpa.rs` 与 `mt/bergamot.rs` 都受 `#[cfg(feature = ...)]` 门控，
//! 而**启动自检必须在 feature 关闭时也能验证模型目录是否就位**
//! —— 否则「模型未安装」这个最重要的提示，要等用户点开始、
//! 等引擎加载 3 秒失败之后才看得见。
//!
//! 所以校验规则单独放这里，由两侧的 `load()` 共同调用，
//! 保证「自检看到的」与「引擎加载时的」是**同一套规则**，不会长期漂移。
//!
//! ## 零依赖
//! 只做文件系统检查，不引入任何 crate。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// ASR（sherpa-onnx transducer）模型文件清单。
///
/// 字段是**解析后的绝对/相对路径**，可直接喂给 `OnlineRecognizerConfig`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsrModelFiles {
    pub encoder: String,
    pub decoder: String,
    pub joiner: String,
    /// 字面精确文件名 `tokens.txt`。上游有时给 `words.txt`，需重命名。
    pub tokens: String,
    /// BPE 词表（`bpe.vocab`），**可选**。
    ///
    /// 中英双语 zipformer（cjkchar+bpe 建模单元）必须提供，
    /// 纯英文模型（cjkchar）没有这个文件 —— 探测到才填。
    pub bpe_vocab: Option<String>,
}

/// MT（Bergamot / Firefox Translations）权重目录的必需文件
///
/// Bergamot 格式（Firefox Translations 同源）实际是：
/// - `model.{lang}.intgemm.alphas.bin` —— **量化权重本体**
///   （int8 量化，文件名里的 "alphas" 是历史遗留，不是"alpha 缩放因子"）
/// - `srcvocab.{lang}.spm` / `trgvocab.{lang}.spm` —— CJK 语言需分离词表
/// - `lex.50.50.{lang}.s2t.bin` —— shortlist（术语/短名单）
///
/// ⚠️ 不存在 `model.bin` / `registry.json` —— 那是 CT2 格式，
/// 本项目已改用 Bergamot，清单必须按 Bergamot 来。
pub const MT_REQUIRED_FILES: [&str; 4] = [
    "model.enzh.intgemm.alphas.bin",
    "srcvocab.enzh.spm",
    "trgvocab.enzh.spm",
    "lex.50.50.enzh.s2t.bin",
];

/// Bergamot 的 yml 配置文件名。
///
/// 配置内列出 models / vocabs / shortlist，相对路径以**配置文件自身
/// 所在目录**为基准解析（marian parser 的 `pathsInSameDirAs` 语义）。
pub const MT_CONFIG_FILE: &str = "config.yml";

/// ASR 目录必须能匹配到的三个 ONNX 前缀
const ASR_STEMS: [&str; 3] = ["encoder", "decoder", "joiner"];

/// 校验 ASR 模型目录，返回解析后的文件路径。
///
/// 约定：目录内含 `encoder*.onnx` / `decoder*.onnx` / `joiner*.onnx` /
/// `tokens.txt`。ONNX 文件名允许量化后缀（如 `encoder-epoch3-99.int8.onnx`）。
///
/// 错误一律用 [`Error::ModelMissing`]，且**消息里带上具体缺什么**——
/// 这条消息会原样显示给用户，不能只说"加载失败"。
pub fn probe_asr_dir(dir: &Path) -> Result<AsrModelFiles> {
    if !dir.is_dir() {
        return Err(Error::ModelMissing(format!(
            "ASR 模型目录不存在: {}",
            dir.display()
        )));
    }

    let mut resolved: Vec<(String, String)> = Vec::with_capacity(ASR_STEMS.len());
    for stem in ASR_STEMS {
        let p = find_best_match(dir, stem).ok_or_else(|| {
            Error::ModelMissing(format!(
                "{stem}*.onnx 未找到（目录: {}）",
                dir.display()
            ))
        })?;
        resolved.push((stem.to_string(), p.display().to_string()));
    }

    // tokens.txt 必须是字面精确名 —— 上游给 words.txt 的情况很常见，
    // 但 sherpa 的分词表文件名不可配置，猜名字会得到静默错误的识别结果。
    let tokens_path = dir.join("tokens.txt");
    if !tokens_path.exists() {
        return Err(Error::ModelMissing(format!(
            "tokens.txt 未找到（目录: {}）。\n\
             注意：必须是字面文件名 tokens.txt，上游包里的 words.txt 需重命名后再放入。",
            dir.display()
        )));
    }

    // 上面的循环按 ASR_STEMS 顺序 push，顺序是确定的
    let get = |stem: &str| -> String {
        resolved
            .iter()
            .find(|(s, _)| s == stem)
            .map(|(_, p)| p.clone())
            .unwrap_or_default()
    };

    // bpe 词表可选：双语（cjkchar+bpe）模型带它，纯英文模型没有。
    // 文件名两种：`bpe.vocab`（旧双语模型）与 `bpe.model`（x-asr 2026 系列
    // 给的 sentencepiece 模型）—— 两者都能喂给 sherpa 的 bpe_vocab 字段。
    let bpe_path = dir.join("bpe.vocab");
    let bpe_path = if bpe_path.exists() { bpe_path } else { dir.join("bpe.model") };
    Ok(AsrModelFiles {
        encoder: get("encoder"),
        decoder: get("decoder"),
        joiner: get("joiner"),
        tokens: tokens_path.display().to_string(),
        bpe_vocab: bpe_path
            .exists()
            .then(|| bpe_path.display().to_string()),
    })
}

/// 校验 MT 权重目录。
///
/// 格式是 **Firefox Translations / bergamot-translator** 的
/// `model.bin` + `registry.json`，**不是** Marian 的 PyTorch 权重
/// （那是 CTranslate2 / opus-mt 的格式，直接喂会失败且报错难懂）。
pub fn probe_mt_dir(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Err(Error::ModelMissing(format!(
            "翻译权重目录不存在: {}",
            dir.display()
        )));
    }
    for f in MT_REQUIRED_FILES {
        if !dir.join(f).exists() {
            return Err(Error::ModelMissing(format!(
                "权重缺少 {f}（目录: {}）。\n\
                 本项目使用 Bergamot 权重格式（Firefox Translations 同源），\n\
                 需从 bergamot-translator 模型仓库获取，而不是 CT2 转换产物。",
                dir.display()
            )));
        }
    }
    // Bergamot 靠 yml 配置加载权重，缺了它引擎根本无法初始化
    let cfg = dir.join(MT_CONFIG_FILE);
    if !cfg.exists() {
        return Err(Error::ModelMissing(format!(
            "权重目录缺少 Bergamot 配置文件 {}（目录: {}）",
            MT_CONFIG_FILE,
            dir.display()
        )));
    }
    Ok(())
}

/// 在 `dir` 内找 `<stem>*.onnx`，同前缀有多个时取路径最大的那个。
///
/// 取最大而非最新修改时间：量化产物常带 `-epoch3-99.int8` 这类后缀，
/// 同一目录里可能同时存在 fp32 与 int8，按名字取最大者行为稳定可复现。
fn find_best_match(dir: &Path, stem: &str) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(stem) && name.ends_with(".onnx") {
            let p = entry.path();
            best = Some(match best {
                Some(prev) if prev < p => prev,
                _ => p,
            });
        }
    }
    best
}

/// ASR 与 MT 模型目录的解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelLayout {
    /// ASR 模型目录（含 encoder/decoder/joiner/tokens）
    pub asr_dir: PathBuf,
    /// MT 权重目录
    pub mt_dir: PathBuf,
}

/// 在若干候选根目录下自动发现 ASR / MT 模型目录。
///
/// 布局约定（安装器把 `models/` 放在 exe 同目录）：
/// ```text
/// {app}/models/asr/<模型名>/encoder*.onnx
/// {app}/models/mt/<模型名>/model.*.bin
/// ```
/// 但开发态目录结构可能不同（`models/asr/<名>/` vs 直接 `models/asr/`），
/// 所以每层都往下找一层。
///
/// 候选根目录按优先级：
/// 1. 显式传入的 `root`
/// 2. exe 同目录的 `models/`
/// 3. exe 同目录的 `../models/`（开发态 target/debug 布局）
/// 4. 当前工作目录的 `models/`
pub fn discover_model_layout(root: Option<&Path>) -> Option<ModelLayout> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(r) = root {
        roots.push(r.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.join("models"));
            roots.push(dir.join("..").join("models"));
            roots.push(dir.join("..").join("..").join("models"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("models"));
    }

    for r in &roots {
        if let Some(layout) = discover_in_root(r) {
            return Some(layout);
        }
    }
    None
}

/// 在一个根目录下找 ASR / MT 子目录。
fn discover_in_root(root: &Path) -> Option<ModelLayout> {
    if !root.is_dir() {
        return None;
    }
    let asr = find_model_dir(&root.join("asr"), "encoder").or_else(|| {
        // 也接受根目录本身就是 ASR 模型目录的情况
        if root.join("encoder").exists() {
            Some(root.to_path_buf())
        } else {
            None
        }
    })?;
    let mt = find_model_dir(&root.join("mt"), "model.").or_else(|| {
        if root.join("model.en").exists() || root.join("config.yml").exists() {
            Some(root.to_path_buf())
        } else {
            None
        }
    })?;
    Some(ModelLayout { asr_dir: asr, mt_dir: mt })
}

/// 在 `dir` 下（或其直接子目录中）找含 `needle` 前缀文件的目录。
fn find_model_dir(dir: &Path, needle: &str) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    // 目录本身就是模型目录
    if has_prefix_file(dir, needle) {
        return Some(dir.to_path_buf());
    }
    // 往下找一层
    let mut found: Option<PathBuf> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() && has_prefix_file(&p, needle) {
            found = Some(p);
            break;
        }
    }
    found
}

/// 目录里是否有以 `prefix` 开头的文件。
fn has_prefix_file(dir: &Path, prefix: &str) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .map(|it| {
            it.flatten().any(|f| {
                f.file_name()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在系统临时目录建一个一次性目录，测试结束自动清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> std::io::Result<Self> {
            let mut p = std::env::temp_dir();
            let uniq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            p.push(format!("translator-probe-{tag}-{uniq}"));
            std::fs::create_dir_all(&p)?;
            Ok(TempDir(p))
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn complete_asr_dir(t: &TempDir) {
        t.write("encoder-epoch3-99.int8.onnx", "x");
        t.write("decoder-epoch3-99.int8.onnx", "x");
        t.write("joiner-epoch3-99.onnx", "x");
        t.write("tokens.txt", "a b c");
    }

    /// 完整的 Bergamot 权重目录（与 `MT_REQUIRED_FILES` 一致）。
    fn complete_mt_dir(t: &TempDir) {
        for f in MT_REQUIRED_FILES {
            t.write(f, "x");
        }
        t.write(MT_CONFIG_FILE, "models: []");
    }

    #[test]
    fn asr_ok_on_complete_dir() {
        let t = TempDir::new("ok").unwrap();
        complete_asr_dir(&t);
        let f = probe_asr_dir(&t.0).expect("完整目录应通过");
        assert!(f.encoder.ends_with("encoder-epoch3-99.int8.onnx"));
        assert!(f.decoder.ends_with("decoder-epoch3-99.int8.onnx"));
        assert!(f.joiner.ends_with("joiner-epoch3-99.onnx"));
        assert!(f.tokens.ends_with("tokens.txt"));
    }

    #[test]
    fn asr_accepts_unquantized_names() {
        let t = TempDir::new("fp32").unwrap();
        t.write("encoder.onnx", "x");
        t.write("decoder.onnx", "x");
        t.write("joiner.onnx", "x");
        t.write("tokens.txt", "a");
        assert!(probe_asr_dir(&t.0).is_ok());
    }

    #[test]
    fn asr_missing_dir_names_the_path() {
        let e = probe_asr_dir(Path::new(r"C:\definitely\not\here")).unwrap_err();
        assert!(matches!(e, Error::ModelMissing(_)));
        // 消息必须带路径，否则用户不知道该往哪放
        assert!(e.to_string().contains("not"));
    }

    #[test]
    fn asr_missing_encoder_says_encoder() {
        let t = TempDir::new("noenc").unwrap();
        t.write("decoder.onnx", "x");
        t.write("joiner.onnx", "x");
        t.write("tokens.txt", "a");
        let e = probe_asr_dir(&t.0).unwrap_err().to_string();
        assert!(e.contains("encoder"), "错误信息须点名缺失文件: {e}");
    }

    #[test]
    fn asr_missing_tokens_explains_rename() {
        // 这是真实会踩的坑：上游给 words.txt
        let t = TempDir::new("words").unwrap();
        t.write("encoder.onnx", "x");
        t.write("decoder.onnx", "x");
        t.write("joiner.onnx", "x");
        t.write("words.txt", "a b c");
        let e = probe_asr_dir(&t.0).unwrap_err().to_string();
        assert!(e.contains("tokens.txt"), "{e}");
        assert!(e.contains("重命名"), "须提示上游文件叫 words.txt: {e}");
    }

    #[test]
    fn mt_ok_on_complete_dir() {
        let t = TempDir::new("mt-ok").unwrap();
        complete_mt_dir(&t);
        assert!(probe_mt_dir(&t.0).is_ok());
    }

    #[test]
    fn mt_missing_registry_names_it() {
        // 缺 Bergamot 权重本体时报错要说清缺哪个文件，
        // 并澄清「不是 CT2 转换产物」—— 用户常误以为要自己转模型。
        let t = TempDir::new("mt-noreg").unwrap();
        t.write("model.bin", "x");
        t.write("registry.json", "{}");
        let e = probe_mt_dir(&t.0).unwrap_err().to_string();
        assert!(e.contains("model.enzh.intgemm.alphas.bin"), "{e}");
        // 必须澄清不是 Marian 权重格式，否则用户会去下 opus-mt
        assert!(e.contains("Firefox Translations") || e.contains("bergamot"), "{e}");
    }

    #[test]
    fn mt_missing_config_is_named() {
        // Bergamot 靠 config.yml 加载权重，缺它引擎无法初始化
        let t = TempDir::new("mt-nocfg").unwrap();
        for f in MT_REQUIRED_FILES {
            t.write(f, "x");
        }
        let e = probe_mt_dir(&t.0).unwrap_err().to_string();
        assert!(e.contains(MT_CONFIG_FILE), "{e}");
    }

    #[test]
    fn probe_is_readonly() {
        // 自检必须是只读的 —— 用户可能在「查看模型」时点过自检，
        // 不该有任何副作用
        let t = TempDir::new("ro").unwrap();
        complete_asr_dir(&t);
        let before = std::fs::read_dir(&t.0).unwrap().count();
        let _ = probe_asr_dir(&t.0);
        assert_eq!(std::fs::read_dir(&t.0).unwrap().count(), before);
    }
}