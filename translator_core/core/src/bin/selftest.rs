//! 流水线自检 CLI。
//!
//! ## 为什么需要它
//!
//! 这是验证「装配线装好了没有」的最快手段：**不需要 GUI、不需要麦克风、
//! 不需要模型权重**，一条命令就能证明整条链路通。
//!
//! 在它出现之前，验证流水线正确性只能靠 `cargo test`，而测试用的是
//! 手写的 mock 文本；现在可以直接喂真实形状的音频，看引擎吐出真实字幕。
//!
//! ## 用法
//!
//! ```text
//! selftest --mock                      # 内置假音频跑通全链路，输出 SRT
//! selftest --mock --seconds 20         # 更长的会话
//! selftest --wav speech.wav --mock     # 用真实录音（16k mono PCM16）
//! selftest --mock --bench              # 额外输出各阶段耗时与 RTF
//! selftest --model-root models --asr-sherpa   # 阶段二：真实模型
//! ```
//!
//! 退出码：0 = 通过；1 = 失败（附原因到 stderr）。
//!
//! ## 阶段一的正常现象
//!
//! `--mock` 模式下 ASR 产出 `word0 word1 word2 …`，MT 按内置 16 词词典翻译。
//! 所以**看到 `word0 word1` 是正常的**；**看不到字幕行才是 bug**。

use std::io::Write;
use std::sync::mpsc;
use std::time::Instant;

use translator_core::audio::PcmChunk;
use translator_core::config::{AppConfig, PerformanceProfile};
use translator_core::pipeline::{
    BackendChoice, PipelineConfig, SessionEngine, StartOutcome, TranscriptSink,
};
use translator_core::transcript::TranscriptWriter;

/// 采集块长，与 `platform/desktop.rs` 的 CHUNK_MS=20 保持一致
const CHUNK_MS: u64 = 20;

/// 在 dir 下自动探测模型目录：
/// - dir 自身含 needle 文件（如 mt/config.yml）→ 返回 dir 自身
/// - 否则找第一个含 `needle` 文件或 `needle*.onnx` 的子目录
///   （如 asr/xxx/encoder-epoch-99-avg-1.int8.onnx，needle="encoder"）
fn discover_dir(dir: &std::path::Path, needle: &str) -> Option<std::path::PathBuf> {
    if dir.join(needle).exists() {
        return Some(dir.to_path_buf());
    }
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(needle).exists() {
            return Some(p);
        }
        if let Ok(files) = std::fs::read_dir(&p) {
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                if name.starts_with(needle) && name.ends_with(".onnx") {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn main() {
    let opts = match Opts::parse() {
        Ok(o) => o,
        Err(e) => fail(&format!("参数错误: {e}\n\n{}", usage())),
    };

    match run(&opts) {
        Ok(true) => std::process::exit(0),
        Ok(false) => std::process::exit(1),
        Err(e) => fail(&e),
    }
}

fn usage() -> &'static str {
    "\
用法: selftest [选项]

  --mock                 用内置参考后端（无需任何模型文件）
  --model-root <dir>     模型根目录（--asr-sherpa / --mt-bergamot 时必需）
  --asr-sherpa           用真实 ASR 引擎
  --mt-bergamot          用真实翻译引擎
  --wav <path>           输入 16kHz mono PCM16 WAV；不给则用内置合成音频
  --seconds <n>          合成音频时长（默认 12 秒）
  --window <n>           内存窗口行数（默认 2000）
  --transcript          用默认转写盘位置（桌面 translate results）
  --transcript-dir <dir> 转写盘输出目录（覆盖默认位置）
  --bench                输出各阶段耗时
  --dump-wav <path>      把合成音频写成 WAV 后退出
"
}

struct Opts {
    mock: bool,
    asr_real: bool,
    mt_real: bool,
    model_root: Option<std::path::PathBuf>,
    wav: Option<std::path::PathBuf>,
    seconds: f32,
    window: usize,
    transcript_dir: Option<std::path::PathBuf>,
    /// 用默认位置（桌面 translate results）
    use_default_transcript: bool,
    bench: bool,
    dump_wav: Option<std::path::PathBuf>,
    /// ASR 推理线程数（0 = 用档位默认）。2026-10-25 用于占用基准测试。
    threads: usize,
    /// 离线精修开关（true = 启用两遍解码第二遍）
    refine: bool,
    /// 按真实节奏喂音频（每 20ms 一块）。占用基准测试必须开 ——
    /// 否则 65 秒音频瞬间灌完，采样窗口太短测不出稳态占用。
    realtime: bool,
}

impl Opts {
    fn parse() -> Result<Self, String> {
        let mut o = Opts {
            mock: false,
            asr_real: false,
            mt_real: false,
            model_root: None,
            wav: None,
            seconds: 12.0,
            window: 2_000,
            transcript_dir: None,
            use_default_transcript: false,
            bench: false,
            dump_wav: None,
            threads: 0,
            refine: true,
            realtime: false,
        };
        let a: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < a.len() {
            let need = |i: usize, name: &str| -> Result<String, String> {
                a.get(i + 1)
                    .cloned()
                    .ok_or_else(|| format!("{name} 缺少参数"))
            };
            match a[i].as_str() {
                "--mock" => o.mock = true,
                "--asr-sherpa" => o.asr_real = true,
                "--mt-bergamot" => o.mt_real = true,
                "--bench" => o.bench = true,
                "--transcript" => o.use_default_transcript = true,
                "--model-root" => {
                    o.model_root = Some(need(i, "--model-root")?.into());
                    i += 1;
                }
                "--wav" => {
                    o.wav = Some(need(i, "--wav")?.into());
                    i += 1;
                }
                "--transcript-dir" => {
                    o.transcript_dir = Some(need(i, "--transcript-dir")?.into());
                    i += 1;
                }
                "--dump-wav" => {
                    o.dump_wav = Some(need(i, "--dump-wav")?.into());
                    i += 1;
                }
                "--seconds" => {
                    o.seconds = need(i, "--seconds")?
                        .parse()
                        .map_err(|_| "--seconds 需为数字".to_string())?;
                    i += 1;
                }
                "--window" => {
                    o.window = need(i, "--window")?
                        .parse()
                        .map_err(|_| "--window 需为整数".to_string())?;
                    i += 1;
                }
                "--threads" => {
                    o.threads = need(i, "--threads")?
                        .parse()
                        .map_err(|_| "--threads 需为整数".to_string())?;
                    i += 1;
                }
                "--refine" => {
                    o.refine = need(i, "--refine")?
                        .parse()
                        .map_err(|_| "--refine 需为 true/false".to_string())?;
                    i += 1;
                }
                "--realtime" => o.realtime = true,
                "-h" | "--help" => {
                    println!("{}", usage());
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数 {other}")),
            }
            i += 1;
        }
        if !o.mock && !o.asr_real && !o.mt_real {
            return Err("至少要给一个后端选择：--mock / --asr-sherpa / --mt-bergamot".into());
        }
        Ok(o)
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("[FAIL] {msg}");
    std::process::exit(1)
}

fn run(opts: &Opts) -> Result<bool, String> {
    // ── 1. 准备音频 ──
    let samples: Vec<f32> = match &opts.wav {
        Some(p) => read_wav(p)?,
        None => synth(opts.seconds),
    };
    let audio_sec = samples.len() as f32 / AppConfig::SAMPLE_RATE as f32;

    if let Some(p) = &opts.dump_wav {
        write_wav(p, &samples)?;
        eprintln!("[INFO] 已写出 {} ({} 秒)", p.display(), audio_sec);
        return Ok(true);
    }

    eprintln!(
        "[INFO] 音频 {} 样本 / {:.1} 秒 / {} Hz",
        samples.len(),
        audio_sec,
        AppConfig::SAMPLE_RATE
    );

    // ── 2. 配置 ──
    let backend = if opts.mock {
        BackendChoice::Mock
    } else {
        BackendChoice::Auto
    };
    let root = opts.model_root.clone().unwrap_or_default();
    // 自动探测模型目录：不再硬编码目录名（下载的模型版本会变）。
    // asr/<任意>/ 下含 encoder*.onnx 即可；mt/ 下有 config.yml 即可。
    let asr_model_dir = discover_dir(&root.join("asr"), "config.yml").unwrap_or_else(|| {
        discover_dir(&root.join("asr"), "encoder").unwrap_or_else(|| root.join("asr/zipformer-en-256ms-int8"))
    });
    let mt_model_dir = discover_dir(&root.join("mt"), "config.yml").unwrap_or_else(|| root.join("mt/bergamot-en-zh-tiny"));
    // 线程数：--threads 显式指定时同时覆盖 ASR/MT（MT 上限 6，与档位联动一致）
    let (asr_threads, mt_threads) = if opts.threads > 0 {
        (opts.threads, opts.threads.min(6))
    } else {
        // 默认走 ResourceTier::default()（Full = 8/6）
        (translator_core::config::ResourceTier::default().asr_threads(),
         translator_core::config::ResourceTier::default().mt_threads())
    };
    let cfg = PipelineConfig {
        asr_model_dir,
        mt_model_dir,
        profile: PerformanceProfile::Stable,
        asr_threads,
        mt_threads,
        // 离线精修（两遍解码第二遍）：--refine true 且目录存在才启用
        offline_model_dir: root.join("offline").join("sense-voice-int8"),
        offline_refine: opts.refine,
        memory_window_lines: opts.window,
        backend,
        transcript_dir: opts.transcript_dir.clone(),
        ..Default::default()
    };

    let t_setup = Instant::now();
    let mut engine = SessionEngine::new(&cfg).map_err(|e| format!("引擎创建失败: {e}"))?;
    let setup = t_setup.elapsed();
    eprintln!("[INFO] 引擎装配完成，耗时 {setup:?}");

    // ── 3. 转写盘 ──
    // 显式目录优先；否则 --transcript 用默认位置（桌面 translate results）；
    // 都没有则纯内存。
    let sink: Option<Box<dyn TranscriptSink>> = if let Some(dir) = &opts.transcript_dir {
        match TranscriptWriter::create_in(dir) {
            Ok(w) => {
                eprintln!("[INFO] 转写盘: {}", w.path().display());
                Some(Box::new(w))
            }
            Err(e) => {
                eprintln!("[WARN] 转写盘创建失败（{}），降级为仅内存", e);
                None
            }
        }
    } else if opts.use_default_transcript {
        match TranscriptWriter::create_default() {
            Ok(w) => {
                eprintln!("[INFO] 转写盘: {}", w.path().display());
                Some(Box::new(w))
            }
            Err(e) => {
                eprintln!("[WARN] 默认转写盘位置不可用（{}），降级为仅内存", e);
                None
            }
        }
    } else {
        None
    };

    // ── 4. 起线程灌音频 ──
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    let started = match sink {
        Some(s) => engine.start_with_sink(rx, s),
        None => engine.start(rx),
    };
    match started {
        StartOutcome::Started => {}
        StartOutcome::AlreadyRunning => return Err("引擎已在运行".into()),
    }

    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let t_feed = Instant::now();
    let mut capture_ms: u64 = 0;
    for block in samples.chunks(chunk_len) {
        if tx
            .send(PcmChunk::new(block.to_vec(), capture_ms))
            .is_err()
        {
            break; // 引擎已退出
        }
        capture_ms += CHUNK_MS;
        // --realtime：按真实时间节奏喂（占用基准测试用）。
        // 对齐到「起播时刻 + 已喂时长」，落后才睡，领先直接继续。
        if opts.realtime {
            let target = std::time::Duration::from_millis(capture_ms);
            let elapsed = t_feed.elapsed();
            if target > elapsed {
                std::thread::sleep(target - elapsed);
            }
        }
    }
    let feed_done = t_feed.elapsed();
    drop(tx); // 通知引擎音频结束

    // 事件不多，攒着最后一起取；生产环境是每 100ms 取一次
    std::thread::sleep(std::time::Duration::from_millis(120));
    let events = engine.take_events();

    let summary = engine.stop().map_err(|e| format!("停止失败: {e}"))?;
    let wall = t_feed.elapsed();

    // ── 4. 报告 ──
    let added = events
        .iter()
        .filter(|e| matches!(e, translator_core::pipeline::LineEvent::Added { .. }))
        .count();
    let filled = events
        .iter()
        .filter(|e| matches!(e, translator_core::pipeline::LineEvent::Filled { .. }))
        .count();

    eprintln!("");
    eprintln!("[统计] VAD 段={}  ASR final={}  译文={}  丢弃译文={}  翻译失败={}",
        summary.stats.vad_segments, summary.stats.asr_finals,
        summary.stats.mt_done, summary.stats.mt_dropped, summary.stats.mt_failed);
    eprintln!("[统计] 事件 Added={added} Filled={filled}");
    eprintln!("[统计] 内存内行数={}  转写盘写入={} 写失败={}",
        summary.lines.len(), summary.transcript_written, summary.transcript_failures);
    if let Some(p) = &summary.transcript_path {
        eprintln!("[统计] 转写盘文件: {p}");
    }

    if opts.bench {
        // RTF（real-time factor）= 处理耗时 / 音频时长。越小越快。
        // RTF < 1 表示比实时快；本项目要求 < 0.5 以留足余量。
        let rtf = if audio_sec > 0.0 {
            wall.as_secs_f64() / audio_sec as f64
        } else {
            f64::INFINITY
        };
        eprintln!("[性能] 装配={setup:?}  灌入={feed_done:?}  排空+收尾={:?}  RTF={rtf:.4}",
            wall.saturating_sub(feed_done));
        eprintln!(
            "[性能] 判据: RTF < 0.5（越小越快；>1 表示跟不上实时）"
        );
        if rtf >= 0.5 {
            eprintln!(
                "[WARN] RTF 偏高 —— 真实场景是实时输入，积压会被 AudioChannel(4s) 吸收；\
                 此处一次性灌入大量音频，退化处理才是速度上限"
            );
        }
    }

    // ── 5. 判定 ──
    let mut problems: Vec<String> = Vec::new();
    if summary.stats.asr_finals == 0 {
        problems.push("没有任何 ASR final 产出 —— 采集或断句环节可能没工作".into());
    }
    if summary.lines.is_empty() {
        problems.push("没有任何字幕行".into());
    }
    if opts.mock && summary.stats.mt_done == 0 && summary.stats.asr_finals > 0 {
        problems.push("有 EN 却没有译文 —— MT 线程可能没启动或全部失败".into());
    }
    if let Some(e) = engine.snapshot().error {
        problems.push(format!("引擎内部报错: {e}"));
    }

    if !problems.is_empty() {
        eprintln!();
        for p in &problems {
            eprintln!("[FAIL] {p}");
        }
        eprintln!();
        eprintln!("--- 已产出的部分（可能为空）---");
        print!("{}", summary.srt);
        return Ok(false);
    }

    eprintln!("[PASS] 链路完整：{} 行字幕，{} 行已译", summary.lines.len(), summary.stats.mt_done);
    eprintln!("--- SRT ---");
    print!("{}", summary.srt);
    let _ = std::io::stdout().flush();
    Ok(true)
}

/// 合成「语音 + 静音」交替的音频。
///
/// 频率取 440Hz：VAD 的过零率判据（zcr <= 0.35）下，
/// 16kHz 采样率时 440Hz 的 zcr 约 0.055，安全通过；
/// 同时 rms 远高于 0.008 阈值。
/// 静音间隔 500ms > hangover 300ms，保证每段独立断句。
fn synth(seconds: f32) -> Vec<f32> {
    let sr = AppConfig::SAMPLE_RATE as usize;
    let total = (seconds * sr as f32) as usize;
    let mut out = Vec::with_capacity(total);
    let tone_len = sr * 4 / 5; // 0.8s
    let gap_len = sr / 2; // 0.5s
    let mut phase = 0.0f32;
    let w = 2.0 * std::f32::consts::PI * 440.0 / sr as f32;

    while out.len() < total {
        for _ in 0..tone_len {
            out.push(0.3 * phase.sin());
            phase += w;
        }
        out.extend(std::iter::repeat_n(0.0f32, gap_len));
    }
    out.truncate(total);
    out
}

/// 读 16kHz mono PCM16 WAV
fn read_wav(path: &std::path::Path) -> Result<Vec<f32>, String> {
    let data = std::fs::read(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err(format!("{} 不是合法的 WAV 文件", path.display()));
    }

    let mut pos = 12usize;
    let (mut fmt_ok, mut channels) = (false, 1u16);
    let mut samples_start = data.len();

    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        match id {
            b"fmt " => {
                channels = u16::from_le_bytes(data[body..body + 2].try_into().unwrap());
                let rate = u32::from_le_bytes(data[body + 4..body + 8].try_into().unwrap());
                let bits = u16::from_le_bytes(data[body + 14..body + 16].try_into().unwrap());
                if rate != AppConfig::SAMPLE_RATE {
                    return Err(format!(
                        "采样率必须是 {} Hz，实际 {}",
                        AppConfig::SAMPLE_RATE,
                        rate
                    ));
                }
                if bits != 16 {
                    return Err(format!("只支持 16 位 PCM，实际 {bits} 位"));
                }
                fmt_ok = true;
            }
            b"data" => samples_start = body,
            _ => {}
        }
        pos = body + size + (size & 1); // chunk 按偶数字节对齐
    }

    if !fmt_ok || samples_start >= data.len() {
        return Err("WAV 缺少 fmt 或 data 块".into());
    }
    if channels != 1 {
        return Err(format!("只支持单声道，实际 {channels} 声道"));
    }

    let raw = &data[samples_start..];
    Ok(raw
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect())
}

/// 写 16kHz mono PCM16 WAV（给 --dump-wav / 人工试听用）
fn write_wav(path: &std::path::Path, samples: &[f32]) -> Result<(), String> {
    let pcm: Vec<u8> = samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let data_len = pcm.len() as u32;
    let byte_rate = (AppConfig::SAMPLE_RATE * 2) as u32;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend(b"RIFF");
    w.extend((36 + data_len).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes()); // PCM
    w.extend(1u16.to_le_bytes()); // mono
    w.extend(AppConfig::SAMPLE_RATE.to_le_bytes());
    w.extend(byte_rate.to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend(data_len.to_le_bytes());
    w.extend(&pcm);
    std::fs::write(path, w).map_err(|e| format!("写不了 {}: {e}", path.display()))
}