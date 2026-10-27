#!/usr/bin/env bash
# 安装 Flutter SDK（国内镜像 + 环境变量配置）
#
# 本项目硬约束：构建与运行双离线。但**安装 Flutter 本身**是开发机配置，
# 不违反离线性 —— 那约束针对的是产物运行期（见 offline.rs）。
#
# 用法：
#   scripts/setup_flutter.sh              # 装到 ~/flutter
#   FLUTTER_DIR=/d/sdk/flutter scripts/setup_flutter.sh

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ── 定位 python ─────────────────────────────────────
# 本机 PATH 中可能只有 python3（无 python），WorkBuddy 自管的是 .exe。
# 统一探测，避免脚本在"有 python 却调不到"的情况下假失败。
PYTHON=""
# 注意：本机 PATH 中 python3 可用而 python 不可用，故 python3 优先
for cand in python3 python py; do
  if command -v "$cand" >/dev/null 2>&1; then PYTHON="$cand"; break; fi
done
if [[ -z "$PYTHON" ]]; then
  # 回退：WorkBuddy 自管的 python
  WB_PY="$HOME/.workbuddy/binaries/python/versions/3.13.12/python.exe"
  [[ -f "$WB_PY" ]] && PYTHON="$WB_PY"
fi
if [[ -z "$PYTHON" ]]; then
  echo "[FAIL] 找不到 python（试过 python/python3/py）" >&2
  exit 1
fi
echo "        python: $PYTHON"

# ── 目标路径 ────────────────────────────────────────
FLUTTER_DIR="${FLUTTER_DIR:-$HOME/flutter}"

# ── 国内镜像（实测 200）────────────────────────────
# 2026-10-17 实测：storage.flutter-io.cn 200 / 269874B，与官方字节数一致
MIRROR="https://storage.flutter-io.cn/flutter_infra_release/releases"
OFFICIAL="https://storage.googleapis.com/flutter_infra_release/releases"

echo "[1/4] 查询最新稳定版…"
# 临时文件放 Windows TEMP（Git Bash 的 mktemp 会创建沙箱不允许的混合路径）
TMP="$TEMP"
RELJSON="$TMP/flutter_releases.json"
export FLUTTER_RELJSON="$RELJSON"
mkdir -p "$TMP" 2>/dev/null || true

if ! curl -sf --max-time 30 "$MIRROR/releases_windows.json" -o "$RELJSON"; then
  echo "      镜像失败，回退官方源"
  curl -sf --max-time 30 "$OFFICIAL/releases_windows.json" -o "$RELJSON"
fi

VERSION="$("$PYTHON" -c "
import json, os
p = os.environ['FLUTTER_RELJSON']
d = json.load(open(p, encoding='utf-8'))
h = d['current_release'].get('stable')
r = [x for x in d['releases'] if x['hash'] == h and x['channel'] == 'stable'][0]
print(r['version'])
" 2>/dev/null || true)"

if [[ -z "$VERSION" ]]; then
  echo "[FAIL] 无法解析 Flutter 版本信息" >&2
  echo "       检查: $RELJSON 是否存在且为合法 JSON" >&2
  ls -la "$RELJSON" 2>/dev/null || echo "       文件不存在"
  exit 1
fi
echo "      最新稳定版: $VERSION"

# ── 下载 ────────────────────────────────────────────
if [[ -d "$FLUTTER_DIR/flutter" ]]; then
  echo "[2/4] Flutter 已存在于 $FLUTTER_DIR/flutter，跳过下载"
else
  echo "[2/4] 下载 Flutter $VERSION（约 1.1 GB，需数分钟）…"
  mkdir -p "$FLUTTER_DIR"
  cd "$FLUTTER_DIR"

  URL="$MIRROR/stable/windows/flutter_windows_${VERSION}-stable.zip"
  if ! curl -fL --max-time 1800 "$URL" -o flutter.zip; then
    echo "      镜像下载失败，回退官方源"
    URL="$OFFICIAL/stable/windows/flutter_windows_${VERSION}-stable.zip"
    curl -fL --max-time 1800 "$URL" -o flutter.zip
  fi

  echo "      解压中（约 1-2 分钟）…"
  # ⚠️ 不要用 `tar -xf` 解 Flutter 的 zip：
  #    实测报 "This does not look like a tar archive" —— Git Bash 的 tar
  #    不支持该 zip 的特性（可能是 zip64 或压缩方式差异）。
  # Windows 用 PowerShell 的 Expand-Archive（原生，能处理长路径）。
  # macOS / Linux 仍用 unzip。
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
      powershell.exe -NoProfile -Command \
        "Expand-Archive -Path '$(cygpath -w "$FLUTTER_DIR/flutter.zip")' -DestinationPath '$(cygpath -w "$FLUTTER_DIR")' -Force" \
        || { echo "[FAIL] 解压失败" >&2; exit 1; }
      ;;
    *)
      unzip -q -o flutter.zip || { echo "[FAIL] 解压失败" >&2; exit 1; }
      ;;
  esac
  rm -f flutter.zip
fi

# ── 配置国内镜像（Flutter 自身也要）─────────────────
echo "[3/4] 配置 Flutter 国内镜像…"
BIN="$FLUTTER_DIR/flutter/bin"
export PATH="$BIN:$PATH"

cat > "$FLUTTER_DIR/flutter/.env.sh" <<EOF
# Flutter 国内镜像配置（source 此文件即可）
export PUB_HOSTED_URL=https://pub.flutter-io.cn
export FLUTTER_STORAGE_BASE_URL=https://storage.flutter-io.cn
export PATH="\$HOME/flutter/bin:\$PATH"
EOF
echo "      已写入 $FLUTTER_DIR/flutter/.env.sh（可 source）"

# ── 预热 ────────────────────────────────────────────
echo "[4/4] 首次运行（下载 Dart SDK 等，需数分钟）…"
export PUB_HOSTED_URL="https://pub.flutter-io.cn"
export FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn"

"$FLUTTER_DIR/flutter/bin/flutter" --version 2>&1 | head -5 || true

cat <<EOF

[PASS] Flutter 安装完成：$FLUTTER_DIR/flutter

后续配置（把下面几行加入 ~/.bashrc）：
  export PUB_HOSTED_URL=https://pub.flutter-io.cn
  export FLUTTER_STORAGE_BASE_URL=https://storage.flutter-io.cn
  export PATH="\$HOME/flutter/bin:\$PATH"

然后：
  cd $REPO/ui
  flutter pub get
  flutter doctor -v     # 检查 Android Studio / NDK 是否被识别

EOF