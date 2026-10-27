#!/usr/bin/env python3
"""生成 16kHz mono PCM16 测试音频（WAV）。

用途：
  1. 拿真实模型（sherpa/Bergamot）做阶段二验证时，需要一段**格式确定**的音频；
  2. 人工试听 VAD 断句是否合理。

格式必须是 16 kHz / mono / 16bit —— 链路全链路固定这个规格
（见 core/src/config.rs 的 AppConfig::SAMPLE_RATE），改了就无法验证。

用法：
  python scripts/gen_test_wav.py out.wav            # 默认 30 秒「语音+静音」交替
  python scripts/gen_test_wav.py out.wav --seconds 60
  python scripts/gen_test_wav.py out.wav --mode tone     # 纯正弦（听感明显）
  python scripts/gen_test_wav.py out.wav --mode speech   # 类语音的多段调制
  python scripts/gen_test_wav.py out.wav --mode silence   # 全静音（应产出 0 行）
"""

from __future__ import annotations

import argparse
import math
import struct
import wave
from pathlib import Path

SAMPLE_RATE = 16000


def gen_tone(seconds: float) -> list[float]:
    """440Hz 连续正弦。zcr≈0.055，远低于 VAD 的 0.35 判据，必然被判为语音。"""
    n = int(seconds * SAMPLE_RATE)
    w = 2 * math.pi * 440 / SAMPLE_RATE
    return [0.3 * math.sin(w * i) for i in range(n)]


def gen_bursts(seconds: float, tone_s: float = 0.8, gap_s: float = 0.5) -> list[float]:
    """语音段 + 静音段交替。gap 必须 > hangover(300ms) 才能各自独立断句。"""
    out: list[float] = []
    tone = gen_tone(tone_s)
    gap = [0.0] * int(gap_s * SAMPLE_RATE)
    while len(out) < int(seconds * SAMPLE_RATE):
        out.extend(tone)
        out.extend(gap)
    return out[: int(seconds * SAMPLE_RATE)]


def gen_speech_like(seconds: float) -> list[float]:
    """类语音：基频 + 谐波 + 音节包络，比纯正弦更接近真实语音的频谱。

    VAD 只看 rms 与 zcr，两者都与真实语音同量级，故可正常触发断句；
    sherpa 在这段音频上大概率识别不出有意义的内容（它不是真语音），
    本函数的价值是**验证断句与链路**，不是验证识别准确率。
    """
    n = int(seconds * SAMPLE_RATE)
    out: list[float] = []
    for i in range(n):
        t = i / SAMPLE_RATE
        # 音节包络：4Hz 起伏 + 缓慢基频漂移
        syllable = 0.5 + 0.5 * math.sin(2 * math.pi * 4.0 * t)
        f0 = 120 + 25 * math.sin(2 * math.pi * 0.3 * t)
        w = 2 * math.pi * f0 / SAMPLE_RATE
        v = (
            math.sin(w * i)
            + 0.5 * math.sin(2 * w * i)
            + 0.25 * math.sin(3 * w * i)
        ) / 1.75
        out.append(0.35 * v * syllable)
    return out


def gen_silence(seconds: float) -> list[float]:
    return [0.0] * int(seconds * SAMPLE_RATE)


MODES = {
    "tone": gen_tone,
    "speech": gen_speech_like,
    "silence": gen_silence,
    "bursts": gen_bursts,
}


def main() -> int:
    ap = argparse.ArgumentParser(description="生成自检用 WAV")
    ap.add_argument("out", type=Path, help="输出路径")
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--mode", choices=sorted(MODES), default="bursts")
    args = ap.parse_args()

    samples = MODES[args.mode](args.seconds)
    pcm = bytearray()
    for s in samples:
        v = max(-1.0, min(1.0, s))
        pcm += struct.pack("<h", int(v * 32767))

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(args.out), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(bytes(pcm))

    print(f"[OK] {args.out}  {len(samples)} 样本 / {args.seconds:.1f} 秒 / 16kHz mono PCM16 (mode={args.mode})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())