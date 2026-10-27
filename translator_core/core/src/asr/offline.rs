//! 离线（非流式）ASR —— 两遍解码的**第二遍**。
//!
//! ## 为什么加第二遍（2026-10-25 用户要求「继续提升质量」）
//!
//! 流式 zipformer 为了首字延迟，只能用 960 ms 解码块 + int8 小模型，
//! 错字率明显高于同代非流式模型。两遍解码的思路：
//! - 第一遍：流式识别（**完全不变**），保证字幕跟手；
//! - 第二遍：句子成行后，用本模块的非流式模型**重识别整段音频**
//!   （音频由 VAD 段提供，见 `SpeechSegment::samples`），
//!   结果非空则**替换**流式文本再送翻译。
//!
//! 用户侧效果：最终看到的是修正后的高质量文本；首字延迟零增加，
//! 代价是句末成行多约 0.2~0.5 s（SenseVoice 的 RTF 极低）。
//!
//! ## 模型
//!
//! SenseVoice-small（中英日韩粤，int8，约 230 MB），自带标点与 ITN。
//! 期望目录内容：`model.int8.onnx` + `tokens.txt`。

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 全链路固定采样率
const SAMPLE_RATE: i32 = 16_000;

/// 离线精修器。可 `Clone`（内部 `Arc`）后跨线程共享。
#[derive(Clone)]
pub struct OfflineRefiner {
    recognizer: Option<Arc<OfflineRecognizer>>,
    /// 模型目录（仅用于日志/自检展示）
    model_dir: PathBuf,
}

impl OfflineRefiner {
    /// 从模型目录构建。
    ///
    /// 目录缺失 / 文件不全 / 加载失败一律返回 `None` ——
    /// **不影响主流程**，只是退回单遍流式识别。
    pub fn try_new(model_dir: &Path, num_threads: usize) -> Option<Self> {
        if !model_dir.is_dir() {
            eprintln!("[offline] 模型目录不存在: {}", model_dir.display());
            return None;
        }
        let model = model_dir.join("model.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        if !model.is_file() {
            eprintln!("[offline] 缺少模型文件: {}", model.display());
            return None;
        }
        if !tokens.is_file() {
            eprintln!("[offline] 缺少词表文件: {}", tokens.display());
            return None;
        }

        let threads = num_threads.max(1) as i32;
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(model.to_string_lossy().into_owned()),
            // auto = 让模型自己判语种（该模型支持中英日韩粤）
            language: Some("auto".to_string()),
            // 开启 ITN：数字/日期做逆文本归一化，字幕更可读
            use_itn: true,
        };
        config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        config.model_config.num_threads = threads;

        let recognizer = OfflineRecognizer::create(&config)?;
        eprintln!(
            "[offline] 精修器就绪: {}（{} 线程）",
            model_dir.display(),
            threads
        );
        Some(Self {
            recognizer: Some(Arc::new(recognizer)),
            model_dir: model_dir.to_path_buf(),
        })
    }

    /// 是否可用（模型加载成功）
    pub fn is_available(&self) -> bool {
        self.recognizer.is_some()
    }

    /// 模型目录
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// 重识别一整段音频，返回修正后的文本。
    ///
    /// - 输入为该 VAD 段的 PCM（16 kHz mono f32）
    /// - 空音频 / 识别失败 / 结果为空 → 返回 `None`，
    ///   **调用方应保留流式结果**（第二遍只许修正，不许清空）
    pub fn refine(&self, samples: &[f32]) -> Option<String> {
        let recognizer = self.recognizer.as_ref()?;
        if samples.is_empty() {
            return None;
        }
        let stream = recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE, samples);
        recognizer.decode(&stream);
        let result = stream.get_result()?;
        let text = result.text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 目录不存在时必须安静失败（不 panic、不阻塞启动）
    #[test]
    fn missing_dir_returns_none() {
        assert!(OfflineRefiner::try_new(Path::new("/no/such/dir"), 2).is_none());
    }
}
