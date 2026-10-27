# 用 Windows 内置 TTS 生成 16kHz mono PCM16 WAV，供 selftest 做真实
# ASR→MT 端到端验证（合成正弦波没有语音内容，ASR 必然 0 产出）。
param(
    [string]$OutPath = "C:\Users\Admin\WorkBuddy\代码开发\Translator\translator_core\tts_test.wav"
)

Add-Type -AssemblyName System.Speech
$fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$synth.Rate = 0
try {
    $synth.SetOutputToWaveFile($OutPath, $fmt)
    $synth.Speak("Good morning everyone.")
    $synth.Speak("The quick brown fox jumps over the lazy dog.")
    $synth.Speak("I am reading a book in the library.")
}
finally {
    $synth.SetOutputToWaveFile([string]::Empty)
    $synth.Dispose()
}

$f = Get-Item $OutPath
"已生成: $OutPath"
"大小: $([math]::Round($f.Length/1024,1)) KB"

# 校验格式（selftest 要求 16kHz mono PCM16）
$sr = $null; $ch = $null; $bits = $null
$bytes = [System.IO.File]::ReadAllBytes($OutPath)
# RIFF 头：22-25 声道数，24-27 采样率，34-35 位深
$ch = [BitConverter]::ToInt16($bytes, 22)
$sr = [BitConverter]::ToInt32($bytes, 24)
$bits = [BitConverter]::ToInt16($bytes, 34)
"采样率: $sr Hz（期望 16000）"
"声道数: $ch（期望 1）"
"位深:   $bits（期望 16）"
if ($sr -ne 16000 -or $ch -ne 1 -or $bits -ne 16) { "[警告] 格式不匹配，selftest 可能拒绝" }
