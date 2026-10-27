//! 自动增益控制（AGC，**带语音门控**）。
//!
//! ## 为什么必须有
//!
//! 2026-10-23 用户反馈「转写仍然不可用」。诊断根因之一：
//! **远场麦克风输入电平偏低**（实测语音帧 RMS 仅 0.002~0.005），
//! 而 `normalize_peak` 只削顶、不提升（peak 低于目标时不动），
//! 所以轻声说话时信号原样进 VAD → 被当静音 → 一段都切不出 → 没有字幕。
//!
//! AGC 把输入电平**自适应**拉到目标水平：
//! - 轻声 → 放大
//! - 响声 → 压制（防削顶）
//! - 环境变化 → 平滑跟随，不突变
//!
//! ## 为什么必须带「语音门控」（2026-10-23 修的关键 bug）
//!
//! 无门控版本：`target_gain = 目标电平 / 当前电平`。
//! 当人**停止说话**后，电平下降到噪声级（0.001），
//! `target_gain` 反被拉到 `MAX_GAIN`（32），
//! **把环境噪声放大 32 倍** —— 键盘声、风扇声全被放大，
//! VAD 误判、ASR 听到嗡嗡声。
//!
//! 门控版：只有检测到「有人在说话」（活动度 > 阈值）才把增益拉到目标；
//! 安静时增益回落到 1.0，不放大底噪。
//!
//! ## 设计（行业标准 RAGC + 语音活动门控）
//!
//! 1. **目标 RMS**：0.1（约 -20 dBFS）。人声舒适区，ASR 识别率最高。
//! 2. **语音活动度**（0..1 包络）：快攻慢放，区分「在说话」与「安静」。
//! 3. **电平跟随**（仅语音活动期间更新）：快攻慢放。
//! 4. **增益限幅**：最大放大 32 倍（+30 dB），最小 0.25（-12 dB）。
//! 5. **增益平滑**：逐块线性逼近目标增益，消除「呼吸感」。

/// 目标 RMS 电平。0.1 ≈ -20 dBFS，人声识别的最佳区间。
const TARGET_RMS: f32 = 0.1;

/// 最大增益（放大）。32 倍 = +30 dB。
///
/// 实测笔记本麦克风**远场**轻声说话 RMS 仅约 0.004，目标是 0.1，
/// 差距 25 倍。MAX_GAIN=32 让远场语音可达 0.128，稳定落在识别区间。
/// 底噪放大风险由语音门控控制（安静时 gain 回落到 1.0，见下）。
const MAX_GAIN: f32 = 32.0;

/// 最小增益（衰减）。0.25 = -12 dB。
/// 输入已经很大（内放/近距离麦克风）时，适度衰减防削顶，
/// 但不过度压缩（保留动态范围）。
const MIN_GAIN: f32 = 0.25;

/// 电平跟随的时间常数（以 20 ms 块为单位）。
/// - 快攻：电平上升时约 2 块（40 ms）内跟上 —— 防削顶优先
/// - 慢放：电平下降时约 50 块（1 秒）内跟上 —— 避免静音段反复拉增益
const ATTACK_BLOCKS: f32 = 2.0;
const RELEASE_BLOCKS: f32 = 50.0;

/// 增益变化的平滑系数（每块最多逼近目标的 15%）。
const GAIN_SMOOTHING: f32 = 0.15;

/// 语音活动门限（**未加增益前的输入 RMS**）。
/// 低于此值视为「没有人在说话」。
///
/// 取值依据：房间底噪/风扇/键盘 RMS 约 0.0003~0.001，
/// 远场人声（即便很轻）约 0.002 以上。取 0.0015 作为分界。
/// 这个门限是绝对值，只负责「别在没人说话时乱放大」；
/// 精细的语音判定由下游 VAD 的自适应噪声底负责。
const ACTIVITY_GATE: f32 = 0.0015;

/// 语音活动度的上升时间常数（块）。快：一有声音立刻拉起增益。
const ACTIVITY_ATTACK: f32 = 2.0;

/// 语音活动度的下降时间常数（块）。慢：人停顿 1~2 秒内增益保持，
/// 避免句内短停顿导致增益反复抖动。50 块 = 1 秒。
const ACTIVITY_RELEASE: f32 = 50.0;

/// 活动度超过此值视为「在说话」，才允许增益 > 1。
const ACTIVITY_ACTIVE: f32 = 0.5;

/// 有状态的自动增益控制器（**带语音门控**）。
///
/// 每个采集源一个实例，跨块保持状态。
#[derive(Debug)]
pub struct Agc {
    /// 当前生效的增益
    gain: f32,
    /// 跟随的**语音**电平（RMS 包络，快攻慢放）
    level: f32,
    /// 语音活动度 0..1（门控状态）。有声音→1，安静→0。
    activity: f32,
    /// 是否已收到第一块（首块直接同步，不做平滑）
    primed: bool,
}

impl Default for Agc {
    fn default() -> Self {
        Self::new()
    }
}

impl Agc {
    pub fn new() -> Self {
        Self {
            gain: 1.0,
            level: 0.0,
            activity: 0.0,
            primed: false,
        }
    }

    /// 处理一块音频：自适应调整增益并应用到样本上。
    ///
    /// **原地**修改 `samples`（零拷贝，音频热路径上避免分配）。
    ///
    /// # 返回
    ///
    /// 应用后的峰值（供诊断 / 削顶检测）。
    pub fn process(&mut self, samples: &mut [f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }

        // 1) 算这块**未加增益前**的真实 RMS
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();

        // 2) 更新语音活动度（门控）：快攻慢放
        //    有声音 → 快速拉满；安静 → 缓慢回落（句内停顿不抖）
        if rms > ACTIVITY_GATE {
            let alpha = 1.0 / ACTIVITY_ATTACK;
            self.activity += alpha * (1.0 - self.activity);
        } else {
            let alpha = 1.0 / ACTIVITY_RELEASE;
            self.activity -= alpha * self.activity;
            if self.activity < 0.0 {
                self.activity = 0.0;
            }
        }

        // 3) 快攻慢放跟随**语音**电平（只在说话时更新，
        //    安静时保持上次的语音电平，避免噪声电平污染估计）
        if self.activity > ACTIVITY_ACTIVE && rms > 1e-5 {
            if rms > self.level {
                let alpha = 1.0 / ATTACK_BLOCKS;
                self.level += alpha * (rms - self.level);
            } else if self.level > 0.0 {
                let alpha = 1.0 / RELEASE_BLOCKS;
                self.level += alpha * (rms - self.level);
            }
        }

        // 4) 目标增益
        //    - 在说话（活动度高）：把电平拉到目标
        //    - 安静（活动度低）：增益回落到 1.0，**不放大底噪**
        let target_gain = if self.activity > ACTIVITY_ACTIVE && self.level > 1e-5 {
            (TARGET_RMS / self.level).clamp(MIN_GAIN, MAX_GAIN)
        } else {
            1.0
        };

        // 5) 增益平滑逼近（首块直接同步）
        if self.primed {
            let step = (target_gain - self.gain).abs() * GAIN_SMOOTHING;
            self.gain += if target_gain > self.gain { step } else { -step };
        } else {
            self.gain = target_gain;
            self.primed = true;
        }

        // 6) 应用到样本
        let g = self.gain;
        let mut peak = 0.0f32;
        for s in samples.iter_mut() {
            *s *= g;
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        peak
    }

    /// 当前增益（诊断用）
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// 语音活动度（诊断用）0..1
    pub fn activity(&self) -> f32 {
        self.activity
    }

    /// 重置（切换声源 / 重新开始转写时调用）
    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.level = 0.0;
        self.activity = 0.0;
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// 构造一块正弦（amp 控制电平）。320 样本 = 20 ms @16k
    fn block(amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.05).sin()).collect()
    }

    #[test]
    fn quiet_speech_gets_amplified() {
        // 远场轻声：amp=0.005 → RMS≈0.0035（> ACTIVITY_GATE，会触发门控）
        // 目标增益 = 0.1 / 0.0035 ≈ 28，在 MAX_GAIN=32 内
        let mut agc = Agc::new();
        let mut samples = block(0.005, 320);
        // 持续喂「语音」（活动度拉满），让增益收敛
        for _ in 0..80 {
            let mut b = block(0.005, 320);
            agc.process(&mut b);
        }
        assert!(agc.activity() > 0.9, "活动度应拉满, activity={}", agc.activity());
        assert!(agc.gain() > 10.0, "gain={}", agc.gain());
        agc.process(&mut samples);
        let out_rms = rms(&samples);
        assert!(
            (out_rms - TARGET_RMS).abs() < 0.03,
            "out_rms={out_rms}, 目标 {TARGET_RMS}"
        );
    }

    #[test]
    fn loud_speech_gets_attenuated() {
        // 较响输入：amp=0.3 → RMS≈0.21，目标增益 = 0.1/0.21 ≈ 0.48
        let mut agc = Agc::new();
        let mut samples = block(0.3, 320);
        for _ in 0..80 {
            let mut b = block(0.3, 320);
            agc.process(&mut b);
        }
        assert!(agc.gain() < 0.6, "gain={}", agc.gain());
        agc.process(&mut samples);
        let out_rms = rms(&samples);
        assert!(
            (out_rms - TARGET_RMS).abs() < 0.03,
            "out_rms={out_rms}, 目标 {TARGET_RMS}"
        );
    }

    #[test]
    fn silence_does_not_amplify_noise() {
        // **核心回归**：人停止说话后，增益必须回落到 1.0，
        // 而不是被拉到 MAX_GAIN 放大环境噪声。
        let mut agc = Agc::new();
        // 先说话让增益拉起来
        for _ in 0..80 {
            let mut b = block(0.005, 320);
            agc.process(&mut b);
        }
        assert!(agc.gain() > 10.0, "说话时应放大, gain={}", agc.gain());

        // 然后安静 200 块 = 4 秒（远长于 ACTIVITY_RELEASE=1秒，
        // 活动度与增益都应完全回落）
        for _ in 0..200 {
            let mut silence = vec![0.0005f32; 320]; // 极轻微底噪
            agc.process(&mut silence);
        }
        // 活动度应已回落，增益应回落到 1.0 附近（绝不放大噪声）
        assert!(agc.activity() < 0.1, "安静时活动度应回落, activity={}", agc.activity());
        assert!(
            agc.gain() < 2.0,
            "安静时增益应回落，不能放大噪声, gain={}",
            agc.gain()
        );
    }

    #[test]
    fn noise_below_gate_is_not_amplified() {
        // 低于门限的底噪（< ACTIVITY_GATE=0.0015）绝不被放大
        let mut agc = Agc::new();
        for _ in 0..200 {
            let mut noise = vec![0.001f32; 320]; // RMS=0.001 < 门限
            agc.process(&mut noise);
        }
        assert!(
            agc.gain() < 2.0,
            "低于门限的噪声不应被放大, gain={}",
            agc.gain()
        );
    }

    #[test]
    fn gain_is_clamped() {
        // 极弱但高于门限的信号：增益不能超过 MAX_GAIN
        let mut agc = Agc::new();
        for _ in 0..300 {
            let mut b = block(0.002, 320);
            agc.process(&mut b);
        }
        assert!(agc.gain() <= MAX_GAIN + 1e-3, "gain={}", agc.gain());
    }

    #[test]
    fn gain_is_smooth_not_jumpy() {
        // 「平滑」：电平突变频时增益渐进逼近，而非瞬时跳变
        let mut agc = Agc::new();
        // 阶段 1：稳定中等电平收敛
        for _ in 0..120 {
            let mut b = block(0.3, 320);
            agc.process(&mut b);
        }
        let g_converged = agc.gain();
        // 阶段 2：电平突降
        let mut one = block(0.05, 320);
        agc.process(&mut one);
        let g_after_1 = agc.gain();
        for _ in 0..300 {
            let mut b = block(0.05, 320);
            agc.process(&mut b);
        }
        let g_new_target = agc.gain();
        // 方向对：电平降 → 增益升
        assert!(g_new_target > g_converged, "应提升增益: {g_converged} -> {g_new_target}");
        // 渐进：1 块内不应直接跳到位
        let gap_converged = (g_new_target - g_converged).abs();
        let gap_one_block = (g_after_1 - g_converged).abs();
        assert!(
            gap_one_block < gap_converged * 0.6,
            "1 块内移动幅度过大，增益不够平滑"
        );
    }

    #[test]
    fn reset_restores_identity() {
        let mut agc = Agc::new();
        for _ in 0..40 {
            let mut b = block(0.005, 320);
            agc.process(&mut b);
        }
        assert!(agc.gain() != 1.0);
        agc.reset();
        assert_eq!(agc.gain(), 1.0);
        assert_eq!(agc.activity(), 0.0);
    }

    #[test]
    fn empty_input_is_safe() {
        let mut agc = Agc::new();
        let empty = &mut [] as &mut [f32];
        assert_eq!(agc.process(empty), 0.0);
        let mut b = block(0.1, 320);
        agc.process(&mut b);
        assert!(agc.gain() > 0.0);
    }
}
