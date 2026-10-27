#!/usr/bin/env bash
# Git 网络通道自动适配包装器
#
# 解决的问题
# ----------
# Windows 上代理软件常被随手开关，端口也随软件版本变化。把某个端口
# 写死进 .git/config 的 http.proxy 后，一旦用户关掉代理软件或换了端口，
# git push 立刻失败，且必须手工改配置才能恢复。
#
# 本脚本的做法
# ------------
#   1. 调用 net_probe.py **实测**当前哪条通道可用
#      （不靠"端口在不在"——沙箱/工具注入的代理端口常在监听却走不通）；
#   2. 把选中的通道通过 `git -c http.proxy=... -c https.proxy=...`
#      **仅对本次命令生效**；
#   3. 全程**不写** .git/config、**不碰**系统代理设置（注册表只读）。
#
# 用法
# ----
#   bash Windows/scripts/git_net.sh push                # 探测 + push
#   bash Windows/scripts/git_net.sh pull                # 探测 + pull
#   bash Windows/scripts/git_net.sh fetch --tags        # 探测 + fetch
#   bash Windows/scripts/git_net.sh ls-remote           # 探测 + ls-remote
#   bash Windows/scripts/git_net.sh probe               # 只探测，不执行 git
#   bash Windows/scripts/git_net.sh -- push             # 跳过探测，直接走直连
#   bash Windows/scripts/git_net.sh --proxy http://127.0.0.1:7890 push
#                                                       # 指定代理，跳过探测
#
# 也可作为 git 的包装直接用：
#   bash Windows/scripts/git_net.sh status              # 任意 git 子命令

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE_PY="$SCRIPT_DIR/net_probe.py"

# 探测用的 python（Windows 上 python/py 名字不统一）
PY=""
for c in python python3 py; do
  if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[FAIL] 找不到 python，无法探测网络通道" >&2; exit 127; }

log() { echo "[git-net] $*" >&2; }

usage() {
  sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

# ── 参数解析 ──────────────────────────────────────────────
MODE_PROXY=""      # 指定代理（--proxy）
MODE_DIRECT=0     # 强制直连（--）
DO_PROBE=1        # 是否探测

if [ $# -gt 0 ]; then
  case "$1" in
    -h|--help) usage ;;
    --)
      MODE_DIRECT=1; DO_PROBE=0; shift ;;
    --proxy)
      shift
      MODE_PROXY="${1:-}"; DO_PROBE=0; shift ;;
  esac
fi

# "probe" 是独立的只探测子命令；其余情况（无参数或任意 git 子命令）
# 都应先探测再执行。
IS_PROBE_ONLY=0
if [ $# -eq 0 ] || [ "${1:-}" = "probe" ]; then
  IS_PROBE_ONLY=1
  # probe 模式下把 "probe" 这个参数去掉，避免被当成 git 子命令
  [ $# -gt 0 ] && shift
fi

if [ "$DO_PROBE" -eq 1 ]; then
  log "探测可用网络通道…"
  OUT="$("$PY" "$PROBE_PY" 2>&1)"
  echo "$OUT" >&2
  RESULT="$(printf '%s\n' "$OUT" | grep -E '^RESULT=' | tail -1)"
  case "$RESULT" in
    RESULT=proxy\ *)
      MODE_PROXY="${RESULT#RESULT=proxy }"
      log "选中代理: $MODE_PROXY" ;;
    RESULT=direct)
      MODE_DIRECT=1
      log "选中直连（不使用代理）" ;;
    *)
      log "无可用通道。请启动代理软件后重试，或检查网络。"
      exit 1 ;;
  esac
fi

# probe 模式到此结束，不执行 git
[ "$IS_PROBE_ONLY" -eq 1 ] && exit 0

if [ $# -eq 0 ]; then
  log "未指定 git 子命令。用法见 --help。"
  exit 2
fi

# ── 组装 git 命令 ─────────────────────────────────────────
# 关键点 1：http.proxy 与 https.proxy **必须同时**设置。
#   - 设了具体值 → 走该代理；
#   - 赋空字符串 → 禁用该层代理。
#   只设 http.proxy= 空值时，git 仍会回落到环境变量里的 https_proxy
#   （实测会继续走它并超时），所以两个都要给。
#
# 关键点 2：走代理时**保留**环境变量里的 http_proxy。
#   实测 Git for Windows 只有在环境变量 http_proxy 存在时才会对
#   https 目标使用 CONNECT 隧道；清空环境变量后即使显式给了
#   -c http.proxy=，隧道也会失败（报 "option to transmit"）。
#   → 选代理：保留环境变量；选直连：清空环境变量（避免 git 回落过去）。
GIT_CFG=(-c "http.proxy=" -c "https.proxy=")
if [ -n "$MODE_PROXY" ]; then
  GIT_CFG+=(-c "http.proxy=$MODE_PROXY" -c "https.proxy=$MODE_PROXY")
  # Git for Windows 的 CONNECT 隧道要求环境变量里有 http_proxy，
  # 所以把选中的代理同步写进环境变量（大小写两种都写）。
  http_proxy="$MODE_PROXY";  https_proxy="$MODE_PROXY"
  HTTP_PROXY="$MODE_PROXY";  HTTPS_PROXY="$MODE_PROXY"
  export http_proxy https_proxy HTTP_PROXY HTTPS_PROXY
  log "以代理 ${MODE_PROXY} 执行 git $*"
else
  # 直连：清掉环境变量里的代理，避免 git 回落过去
  unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY ALL_PROXY all_proxy 2>/dev/null || true
  log "以直连方式执行 git $*"
fi

exec git "${GIT_CFG[@]}" "$@"
