#!/usr/bin/env bash
# 更新所有工具的 Android SDK 路径识别（迁移到 D 盘后运行一次）
#
# ## 迁移背景
# C 盘空间紧张（原 4.1 GB SDK 占 C 盘），已迁到 D:\Android\Sdk。
# 本脚本把**所有引用旧路径的地方**改到新路径。
#
# ## 覆盖范围
# 1. Windows 用户环境变量：ANDROID_HOME / ANDROID_SDK_ROOT / ANDROID_NDK_HOME / PATH
# 2. Android Studio：options/android.sdk.path.xml + local.properties
# 3. 工程脚本：scripts/*.sh 里的 SDK 探测逻辑
#
# 用法：bash Android/scripts/update_sdk_paths.sh [新SDK路径]
#   默认新路径：D:\Android\Sdk

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$AND_DIR/.." && pwd)"
REPO="$ROOT/translator_core"
OLD_SDK_WIN='C:\Users\Admin\AppData\Local\Android\Sdk'
NEW_SDK_WIN="${1:-D:\Android\Sdk}"

# Git Bash 风格路径（用于脚本内探测）
NEW_SDK_MSYS="$(echo "$NEW_SDK_WIN" | sed 's|\\|/|g; s|^\([A-Za-z]\):|/\L\1|')"

echo "=============================================="
echo " Android SDK 路径迁移更新"
echo "=============================================="
echo "  旧: $OLD_SDK_WIN"
echo "  新: $NEW_SDK_WIN"
echo ""

# ── 1. Windows 用户环境变量 ───────────────────────────
echo "[1/4] 更新 Windows 用户环境变量…"
export PATH="/c/Windows/System32:/c/Windows:$PATH"

set_env_var() {
  local name="$1" value="$2"
  # setx 有 1024 字符限制且对空值处理差，故用 [Environment]::SetEnvironmentVariable
  powershell -NoProfile -Command "
    [Environment]::SetEnvironmentVariable('$name', '$value', 'User')
    Write-Output '    $name = $value'
  " 2>/dev/null || echo "    [WARN] $name 设置失败（可能需手动设）"
}

set_env_var "ANDROID_HOME"        "$NEW_SDK_WIN"
set_env_var "ANDROID_SDK_ROOT"    "$NEW_SDK_WIN"

# NDK 路径（自动探测最新版）
# NDK 探测（set -u 下用 := 形式避免未绑定）
NDK_DIR="$NEW_SDK_MSYS/ndk"
if [[ -d "$NDK_DIR" ]]; then
  NDK_VER="$(ls -1 "$NDK_DIR" 2>/dev/null | sort -V | tail -1)"
  if [[ -n "${NDK_VER:-}" ]]; then
    set_env_var "ANDROID_NDK_HOME" "${NEW_SDK_WIN}\\ndk\\${NDK_VER}"
    echo "    (NDK 版本: $NDK_VER)"
  else
    echo "    [WARN] NDK 目录为空，跳过 ANDROID_NDK_HOME"
  fi
else
  echo "    [WARN] 未探测到 NDK，跳过 ANDROID_NDK_HOME"
fi

# PATH：把旧 SDK 路径相关项换成新的
powershell -NoProfile -Command "
  \$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not \$userPath) { \$userPath = '' }
  \$old = '$OLD_SDK_WIN'
  \$new = '$NEW_SDK_WIN'
  \$parts = \$userPath -split ';' | Where-Object { \$_ -ne '' }
  # 移除指向旧 SDK 的条目
  \$parts = \$parts | Where-Object { \$_ -notlike \"\$old*\" }
  # 追加新 SDK 的关键 bin 目录（去重）
  \$toAdd = @(
    \"\$new\platform-tools\",
    \"\$new\cmdline-tools\latest\bin\",
    \"\$new\emulator\"  # 已删除，加了无害
  )
  foreach (\$a in \$toAdd) { if (\$parts -notcontains \$a) { \$parts += \$a } }
  \$newPath = (\$parts -join ';')
  [Environment]::SetEnvironmentVariable('Path', \$newPath, 'User')
  Write-Output '    PATH 已更新（移除旧 SDK 项，追加新 SDK bin）'
" 2>/dev/null || echo "    [WARN] PATH 更新失败"

# ── 2. Android Studio 识别 ──────────────────────────
echo ""
echo "[2/4] 更新 Android Studio SDK 路径识别…"
AS_OPT="$APPDATA/Google/AndroidStudio2026.1.4/options"
if [[ -d "$AS_OPT" ]]; then
  cat > "$AS_OPT/android.sdk.path.xml" <<XML
<application>
  <component name="AndroidSdkPathStore">
    <option name="androidSdkAbsolutePath" value="$NEW_SDK_WIN" />
  </component>
</application>
XML
  echo "    android.sdk.path.xml -> $NEW_SDK_WIN"
else
  echo "    [WARN] AS 配置目录不存在（AS 未装或版本不同）: $AS_OPT"
fi

# 工程 local.properties（Gradle 用）
LP="$REPO/ui/android/local.properties"
cat > "$LP" <<PROPS
# 本地配置，勿提交到版本库
sdk.dir=$(echo "$NEW_SDK_WIN" | sed 's|\\|\\\\|g')
PROPS
echo "    local.properties -> $NEW_SDK_WIN"

# ── 3. 工程脚本探测逻辑 ──────────────────────────────
echo ""
echo "[3/4] 更新工程脚本 SDK 探测…"
# build_android.sh 的探测链：ANDROID_NDK_HOME → SDK/ndk，已兼容任意路径。
# check_android_env.py 的默认值指向 ~/AppData/Local，需改为环境变量优先。
sed -i "s|sdk = os.environ.get(\"ANDROID_HOME\") or os.environ.get(\"ANDROID_SDK_ROOT\")|sdk = os.environ.get(\"ANDROID_HOME\") or os.environ.get(\"ANDROID_SDK_ROOT\") or r\"$NEW_SDK_WIN\"|g" \
  "$SCRIPT_DIR/check_android_env.py" 2>/dev/null && echo "    check_android_env.py 默认 SDK 已更新" \
  || echo "    [WARN] check_android_env.py 更新失败"

# ── 4. 验证 ──────────────────────────────────────────
echo ""
echo "[4/4] 验证新路径…"
if [[ -d "$NEW_SDK_MSYS/platforms/android-34" ]]; then
  echo "    [OK] platforms/android-34"
else
  echo "    [FAIL] 找不到 platforms/android-34"
fi
if [[ -d "$NEW_SDK_MSYS/ndk" ]]; then
  echo "    [OK] ndk/$(ls -1 "$NEW_SDK_MSYS/ndk" | tr '\n' ' ')"
else
  echo "    [FAIL] 找不到 ndk"
fi
if [[ -d "$NEW_SDK_MSYS/build-tools/36.0.0" ]]; then
  echo "    [OK] build-tools/36.0.0"
else
  echo "    [FAIL] 找不到 build-tools/36.0.0"
fi
if [[ -f "$NEW_SDK_MSYS/cmdline-tools/latest/bin/android.exe" ]]; then
  echo "    [OK] android CLI"
else
  echo "    [FAIL] 找不到 android CLI"
fi

cat <<EOF

==============================================
 完成
==============================================

已更新：
  1. 环境变量 ANDROID_HOME / ANDROID_SDK_ROOT / ANDROID_NDK_HOME / PATH
  2. Android Studio options/android.sdk.path.xml
  3. 工程 ui/android/local.properties
  4. Android/scripts/check_android_env.py 默认路径

⚠️ 环境变量只在**新开的终端**生效。当前终端请执行：
  export ANDROID_HOME="$NEW_SDK_MSYS"
  export ANDROID_SDK_ROOT="$NEW_SDK_MSYS"
  export PATH="\$ANDROID_HOME/platform-tools:\$ANDROID_HOME/cmdline-tools/latest/bin:\$PATH"

下一步：
  cd "$REPO" && python "$SCRIPT_DIR/check_android_env.py"
EOF