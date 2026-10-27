#!/usr/bin/env bash
# 真机部署：构建产物安装 + 模型推送 + 冒烟检查。
#
# ## 前置
#   1) 手机开启「USB 调试」并已连接（adb devices 能看到）
#   2) 已构建：bash Android/scripts/build_android.sh --release
#              + flutter build apk --debug
#
# ## 模型推送
# 模型不进 APK（162MB 会让安装极慢），用 adb push 送到应用外部私有目录：
#   /sdcard/Android/data/com.translator.app.debug/files/models/
#
# ⚠️ 包名带 `.debug` 后缀（build.gradle.kts 的 applicationIdSuffix），
#    装机后包名不同，push 路径也要跟着变 —— 本脚本自动按实际包名推导。
#
# ## 用法
#   bash Android/scripts/deploy_device.sh            # 安装 + 推 ASR 模型
#   bash Android/scripts/deploy_device.sh --mt       # 连 MT 权重一起推（阶段2）
#   bash Android/scripts/deploy_device.sh --no-push  # 只装不推模型
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$AND_DIR/.." && pwd)"
REPO="$ROOT/translator_core"
ADB="${ADB:-D:/Android/Sdk/platform-tools/adb.exe}"
PKG="com.translator.app.debug"

PUSH_MT=0
PUSH_MODELS=1
for arg in "$@"; do
  case "$arg" in
    --mt) PUSH_MT=1 ;;
    --no-push) PUSH_MODELS=0 ;;
    *) echo "未知参数: $arg" >&2; exit 1 ;;
  esac
done

# ── 设备检查 ──────────────────────────────────────────
DEVICES="$("$ADB" devices | tail -n +2 | awk '{print $1}' | grep -v '^$' || true)"
if [[ -z "$DEVICES" ]]; then
  cat >&2 <<'EOF'
[FAIL] 没有检测到连接的 Android 设备。

请按顺序检查：
  1) 手机开启「设置 → 关于手机 → 连点版本号 7 次」→ 开发者选项
  2) 开发者选项里勾选「USB 调试」
  3) 用数据线连电脑（**不能用只供电的充电线**）
  4) 手机上弹出「允许 USB 调试吗」→ 勾「始终允许」→ 确定
  5) Windows 侧装好厂商 USB 驱动（小米/华为/OPPO 各不相同）

验证：adb devices -l  （应看到 device 而非 unauthorized/offline）
EOF
  exit 1
fi
SERIAL="$(echo "$DEVICES" | head -1)"
echo "[INFO] 设备: $SERIAL"
"$ADB" -s "$SERIAL" wait-for-device

# ── 安装 APK ───────────────────────────────────────────
APK="$REPO/ui/build/app/outputs/flutter-apk/app-debug.apk"
if [[ ! -f "$APK" ]]; then
  echo "[FAIL] 未找到 APK: $APK" >&2
  echo "       先执行：cd translator_core/ui && flutter build apk --debug" >&2
  exit 1
fi
echo "[INFO] 安装 $(basename "$APK") ..."
"$ADB" -s "$SERIAL" install -r -t "$APK"

# ── 推送模型 ───────────────────────────────────────────
if [[ "$PUSH_MODELS" == "1" ]]; then
  TARGET="/sdcard/Android/data/$PKG/files/models"
  echo "[INFO] 推送模型到 $TARGET"
  "$ADB" -s "$SERIAL" shell mkdir -p "$TARGET" 2>/dev/null || true
  # ASR：整目录推（含下一层的模型子目录）
  "$ADB" -s "$SERIAL" push "$REPO/models/asr" "$TARGET/" >/dev/null
  echo "  [OK] ASR 模型"
  if [[ "$PUSH_MT" == "1" ]]; then
    "$ADB" -s "$SERIAL" push "$REPO/models/mt" "$TARGET/" >/dev/null
    echo "  [OK] MT 权重"
  else
    echo "  [--] MT 权重未推（阶段1：仅识别模式，UI 会明示「翻译引擎未启用」）"
  fi
  # 词库（可选）
  if [[ -d "$REPO/models/glossary" ]]; then
    "$ADB" -s "$SERIAL" push "$REPO/models/glossary" "$TARGET/" >/dev/null
    echo "  [OK] 词库"
  fi
  echo "[INFO] 推送完成（首次 162MB，视 USB 速度约 10-60 秒）"
fi

# ── 冒烟检查 ───────────────────────────────────────────
echo ""
echo "[INFO] 启动应用"
"$ADB" -s "$SERIAL" shell monkey -p "$PKG" -c android.intent.category.LAUNCHER 1 >/dev/null 2>&1 || \
  "$ADB" -s "$SERIAL" shell am start -n "$PKG/.MainActivity" >/dev/null 2>&1 || true
sleep 3
echo "[INFO] 进程状态："
"$ADB" -s "$SERIAL" shell pidof "$PKG" || echo "  （未运行 —— 看下面的 logcat 找原因）"
echo ""
echo "[INFO] 最近日志（Ctrl+C 退出）："
echo "  adb -s $SERIAL logcat -s TranslatorCapture:* TranslatorBridge:* flutter:* libc:*"
"$ADB" -s "$SERIAL" logcat -d -t 200 2>/dev/null | grep -iE "translator|flutter|AndroidRuntime|FATAL" | tail -30 || true
