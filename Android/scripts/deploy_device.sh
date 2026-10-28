#!/usr/bin/env bash
# 真机部署：构建产物安装 + 模型推送 + 冒烟检查。
#
# ## 前置
#   1) 手机开启「USB 调试」并已连接（adb devices 能看到）
#   2) 已构建：bash Android/scripts/build_android.sh --release
#              + flutter build apk --debug
#
# ## 模型推送
# 模型不进 APK（162MB 会让安装极慢），用 adb push 送到应用**内部私有目录**：
#   /data/data/com.translator.app.debug/files/models/
#
# 流程是「push 到 /data/local/tmp → run-as 拷进内部目录」，两个原因：
#   1) 大文件直推 /sdcard 会 EOF 中断（Android 14 FUSE 限制）
#   2) 直推 /sdcard 的文件属主是 shell，应用读不到（errno 13）
# 详见下方「推送模型」段的完整说明。
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
#
# ## 为什么不用「adb push 直接到应用目录」
#
# 两个硬约束（都是本机实测踩过的坑）：
#
# 1) **大文件推 /sdcard 会 EOF 中断**
#    "adb push: copy response: EOF" —— Android 14 的 FUSE/sdcardfs 对
#    大文件写入的限制，与 USB 速度无关（155MB / 3.7 秒可推 tmp，推 sdcard 即断）。
#    → 必须先推 /data/local/tmp 再在设备侧搬。
#
# 2) **推 /sdcard 后应用读不到（errno 13）**
#    `adb push` 的文件属主是 **shell**，目录权限 `drwxrws--- shell ext_data_rw`；
#    应用（属主 u0_xxx）既不是 owner 也不在 ext_data_rw 组 → 访问被拒：
#      "FileSystemException: Exists failed ... Permission denied, errno = 13"
#    而且 `Directory.existsSync()` 在无权访问时**抛异常**而非返回 false
#    → 整个 bootstrap 被中断（界面永远「加载中」）。
#
# → 正确做法：**推 /data/local/tmp → `run-as` 拷进内部私有目录**。
#    内部目录属主天然是应用 UID，无需任何 chmod。
#
if [[ "$PUSH_MODELS" == "1" ]]; then
  STAGE="/data/local/tmp/tmodels"
  echo "[INFO] 推送模型（先到 $STAGE，再 run-as 搬进应用私有目录）"
  "$ADB" -s "$SERIAL" shell "rm -rf $STAGE && mkdir -p $STAGE" >/dev/null 2>&1 || true

  push_model() {
    local src="$1" label="$2"
    if [[ ! -d "$src" ]]; then
      echo "  [--] $label 不存在（$src），跳过"
      return 0
    fi
    echo "  [..] $label → $STAGE"
    # -r 递归；失败不退出脚本（后面统一报错）
    if ! "$ADB" -s "$SERIAL" push "$src" "$STAGE/" >/dev/null 2>&1; then
      echo "  [FAIL] $label 推送失败"
      return 1
    fi
    echo "  [OK] $label 已到 tmp"
    return 0
  }

  FAIL=0
  push_model "$REPO/models/asr" "ASR 模型" || FAIL=1
  if [[ "$PUSH_MT" == "1" ]]; then
    push_model "$REPO/models/mt" "MT 权重" || FAIL=1
  else
    echo "  [--] MT 权重未推（阶段1：仅识别模式，UI 会明示「翻译引擎未启用」）"
  fi
  push_model "$REPO/models/glossary" "词库" || FAIL=1

  if [[ "$FAIL" == "1" ]]; then
    echo "[FAIL] 模型推送失败，未执行搬运" >&2
    exit 1
  fi

  echo "[INFO] 搬运到应用内部私有目录（run-as，属主=应用 UID）"
  "$ADB" -s "$SERIAL" shell "run-as $PKG mkdir -p files/models" >/dev/null 2>&1 || true
  for name in asr glossary; do
    if "$ADB" -s "$SERIAL" shell "run-as $PKG sh -c 'test -d $STAGE/$name'" >/dev/null 2>&1; then
      if "$ADB" -s "$SERIAL" shell "run-as $PKG sh -c 'rm -rf files/models/$name && cp -r $STAGE/$name files/models/'" >/dev/null 2>&1; then
        SIZE=$("$ADB" -s "$SERIAL" shell "run-as $PKG du -sh files/models/$name" 2>/dev/null | awk '{print $1}')
        echo "  [OK] $name → files/models/$name ($SIZE)"
      else
        echo "  [FAIL] $name 搬运失败" >&2
        exit 1
      fi
    fi
  done
  if [[ "$PUSH_MT" == "1" ]]; then
    "$ADB" -s "$SERIAL" shell "run-as $PKG sh -c 'rm -rf files/models/mt && cp -r $STAGE/mt files/models/'" >/dev/null 2>&1 && \
      echo "  [OK] mt → files/models/mt" || echo "  [FAIL] mt 搬运失败" >&2
  fi

  # 清 tmp（162MB 别留在设备上）
  "$ADB" -s "$SERIAL" shell "rm -rf $STAGE" >/dev/null 2>&1 || true
  echo "[INFO] 模型就位（应用内部私有目录，无需 chmod）"
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
