//! VAD 与语音分段。
//!
//! 断句**由本模块独占**（§6.2）。sherpa-onnx 的 endpoint 必须关闭，
//! 两套断句并存会把句子切碎。

pub mod segmenter;

pub use segmenter::{Segmenter, SpeechSegment, VadDecision};
