#!/usr/bin/env bash
# 流水线自检：一条命令验证「采集→断句→识别→翻译→字幕」是否打通。
#
# 不需要 GUI、不需要麦克风、不需要模型权重。
# 这是改完 core/ 后最快的反馈回路。
#
# 用法：
#   scripts/selftest.sh                     # mock 后端跑内置假音频
#   scripts/selftest.sh --seconds 60        # 更长的会话
#   scripts/selftest.sh --bench             # 附带性能数据
#   scripts/selftest.sh --wav speech.wav    # 用真实录音
#
# 退出码 0 = 通过。阶段一看到 word0 word1 是正常的（那是 mock ASR 的输出），
# 看不到字幕行才是 bug。

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE="$REPO/core"

export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v cargo >/dev/null 2>&1; then
  echo "[FAIL] 找不到 cargo。Rust 装好后重试。" >&2
  exit 1
fi

# 默认 mock；用户显式给了后端参数就照用
ARGS=()
MOCK=1
for a in "$@"; do
  case "$a" in
    --mock|--asr-sherpa|--mt-bergamot) MOCK=0; ARGS+=("$a") ;;
    *) ARGS+=("$a") ;;
  esac
done
if [[ "$MOCK" == "1" ]]; then
  ARGS=(--mock "${ARGS[@]}")
fi

echo "=== 构建 selftest ==="
( cd "$CORE" && cargo build --bin selftest ) || {
  echo "[FAIL] 编译失败" >&2
  exit 1
}

echo ""
echo "=== 运行自检 ==="
# 用 debug 构建的结果即可，无需 release —— 自检要的是快速反馈
( cd "$CORE" && ./target/debug/selftest.exe "${ARGS[@]}" )