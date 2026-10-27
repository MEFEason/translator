//! SPSC 无锁环形缓冲：捕获线程 → 音频处理线程。
//!
//! 核心约束（DESIGN.md §6.2）：**捕获线程绝不能被下游阻塞**。
//! Windows 上音频回调卡顿会被系统判定为音频故障；macOS 上会触发
//! 系统级音频中断。因此满时**丢弃最旧帧**并计数，宁可丢音频
//! 也不能让实时回调等待。

use crate::audio::PcmChunk;

/// 丢帧计数器，原子读，供健康度上报。
#[derive(Debug, Default)]
pub struct DropCounter(std::sync::atomic::AtomicU64);

impl DropCounter {
    pub fn increment(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.0.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 固定容量的音频通道。
///
/// 容量按**样本数**而非块数计算：各平台捕获块大小不同
/// （Windows ~10 ms、ScreenCaptureKit 20 ms），按块计数会让
/// 实际缓冲时长在三端不一致，破坏 §7.2 的黄金测试前提。
pub struct AudioChannel {
    tx: Option<rtrb::Producer<PcmChunk>>,
    rx: Option<rtrb::Consumer<PcmChunk>>,
    capacity_samples: usize,
    dropped: DropCounter,
}

impl AudioChannel {
    /// `capacity_chunks` 为**块数**容量（rtrb 的槽位语义）。
    ///
    /// 注意：rtrb 以块为粒度，无法按样本数精确控制缓冲时长。
    /// 各平台捕获块长不同（Windows ~10 ms、ScreenCaptureKit 20 ms），
    /// 因此实际缓冲时长会有偏差。为保证三端延迟一致（DESIGN.md §7.2），
    /// 平台采集层必须先归一化块长：`platform/*/capture.rs` 负责把
    /// 自己的原生块长切分成统一的 `CHUNK_MS` 再上送。
    pub const CHUNK_MS: u64 = 20;

    pub fn new(capacity_chunks: usize) -> Self {
        let cap = capacity_chunks.next_power_of_two().max(2);
        let (tx, rx) = rtrb::RingBuffer::<PcmChunk>::new(cap);
        Self {
            tx: Some(tx),
            rx: Some(rx),
            capacity_samples: cap,
            dropped: DropCounter::default(),
        }
    }

    /// 捕获线程调用。**永不阻塞**。
    ///
    /// 缓冲满时丢弃该块并计数 —— 见模块级说明。
    pub fn push(&mut self, chunk: PcmChunk) -> bool {
        let Some(tx) = &mut self.tx else {
            return false;
        };
        if tx.is_full() {
            self.dropped.increment();
            return false;
        }
        tx.push(chunk).is_ok()
    }

    /// 处理线程调用。非阻塞，缓冲空时返回 None。
    pub fn try_pop(&mut self) -> Option<PcmChunk> {
        self.rx.as_mut()?.pop().ok()
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped.get()
    }

    /// 实际槽位容量（块）
    pub fn capacity_samples(&self) -> usize {
        self.capacity_samples
    }

    /// 缓冲时长（毫秒）
    pub fn capacity_ms(&self) -> u64 {
        self.capacity_samples as u64 * Self::CHUNK_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: usize, ms: u64) -> PcmChunk {
        PcmChunk::new(vec![0.1; n], ms)
    }

    #[test]
    fn push_then_pop_preserves_order() {
        let mut ch = AudioChannel::new(1024);
        assert!(ch.push(chunk(100, 0)));
        assert!(ch.push(chunk(100, 10)));
        assert!(ch.push(chunk(100, 20)));

        assert_eq!(ch.try_pop().unwrap().capture_time_ms, 0);
        assert_eq!(ch.try_pop().unwrap().capture_time_ms, 10);
        assert_eq!(ch.try_pop().unwrap().capture_time_ms, 20);
        assert!(ch.try_pop().is_none());
    }

    #[test]
    fn full_buffer_drops_instead_of_blocking() {
        // 容量 2 块，第 3 块必须被丢弃而不是阻塞
        let mut ch = AudioChannel::new(2);
        assert!(ch.push(chunk(320, 0)));
        assert!(ch.push(chunk(320, 1)));
        let start = std::time::Instant::now();
        assert!(!ch.push(chunk(320, 2)));
        assert!(start.elapsed().as_millis() < 50, "push 阻塞了！");
        assert_eq!(ch.dropped_frames(), 1);
    }

    #[test]
    fn empty_channel_returns_none() {
        let mut ch = AudioChannel::new(64);
        assert!(ch.try_pop().is_none());
    }

    #[test]
    fn drop_counter_resets() {
        let mut ch = AudioChannel::new(2);
        ch.push(chunk(320, 0));
        ch.push(chunk(320, 1));
        assert!(!ch.push(chunk(320, 2)));
        assert_eq!(ch.dropped_frames(), 1);
        ch.dropped.reset();
        assert_eq!(ch.dropped_frames(), 0);
    }

    #[test]
    fn capacity_ms_reflects_chunk_granularity() {
        // 容量 256 块 × 20 ms = 5120 ms 缓冲
        let ch = AudioChannel::new(256);
        assert_eq!(ch.capacity_ms(), 5120);
    }
}
