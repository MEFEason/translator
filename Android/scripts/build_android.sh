#!/usr/bin/env bash
# Android 交叉编译 translator_core → .so（含真实 ASR 引擎）
#
# ## 产物去向
#   ui/android/app/src/main/jniLibs/arm64-v8a/
#     ├── libtranslator_core.so      本脚本编译
#     ├── libsherpa-onnx-c-api.so    官方 Android 包（shared 模式）
#     └── libonnxruntime.so          同上（c-api 的 NEEDED）
#
# ## 为什么 Android 用 shared 而非 static
# sherpa-onnx-sys 对 (Static, "android") 直接报 Unsupported target
# （官方不发 Android 静态包）。磁盘带 onnxruntime 文件的禁令是
# Windows 360 误报专项；Android 沙箱隔离，无此约束。
#
# ## 为什么只编 arm64
# 卓易通容器是 arm64，且带其它 ABI 会显著增大包体（core + sherpa-onnx
# + Bergamot 每个 ABI 都要一份）。
#
# ## 前置
#   rustup target add aarch64-linux-android
#   NDK 已安装（SDK/ndk/<version>）
#   sherpa Android 包已落盘 third_party/sherpa-prebuilt/android/
#   （见 fetch_sherpa_android.sh，或手动 curl + 7z 解包）
#
# 用法：
#   Android/scripts/build_android.sh                    # 构建 debug
#   Android/scripts/build_android.sh --release          # 构建 release
#   Android/scripts/build_android.sh --mock             # 仅 mock 后端（调试壳）
#
# 版本注入（与 package.sh 同一规范，勿漏 —— 否则产物停在 snapshot-v0.0.0）：
#   TRANSLATOR_SEMVER=0.9.27 TRANSLATOR_CHANNEL=beta bash build_android.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$AND_DIR/.." && pwd)"
REPO="$ROOT/translator_core"
CORE="$REPO/core"
JNI_LIBS="$REPO/ui/android/app/src/main/jniLibs"
SHERPA_ANDROID="$REPO/third_party/sherpa-prebuilt/android"

export PATH="$HOME/.cargo/bin:$PATH"

# ── 参数 ──────────────────────────────────────────────
PROFILE="debug"
CARGO_FLAGS=""
MOCK_ONLY=0
DO_CLEAN=0
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE="release"; CARGO_FLAGS="--release" ;;
    --clean)   DO_CLEAN=1 ;;
    --mock)    MOCK_ONLY=1 ;;
    *) echo "未知参数: $arg" >&2; exit 1 ;;
  esac
done

# ── 版本注入 ──────────────────────────────────────────
# 缺省沿用 snapshot/0.0.0 时 build.rs 会 warning，这里显式提示，
# 避免"能编但版本全错"（Windows 侧踩过同款坑）。
if [[ -z "${TRANSLATOR_SEMVER:-}" || -z "${TRANSLATOR_CHANNEL:-}" ]]; then
  echo "[WARN] 未设 TRANSLATOR_SEMVER / TRANSLATOR_CHANNEL —— 产物版本将是 snapshot-v0.0.0" >&2
fi
# 时间只取一次（构建耗时长，分段重取会导致三处版本串不一致）
export TRANSLATOR_BUILD_TIME="${TRANSLATOR_BUILD_TIME:-$(date '+%Y-%m-%d %H:%M:%S')}"
# 端标识（全小写，用户 2026-10-28 规定）：版本串格式
#   {端标识}-{渠道}-v{版本}-{时间戳}，例 android-beta-v0.9.7-2026.1028.09.35
# 显式注入而非依赖 build.rs 按 CARGO_CFG_TARGET_OS 推断 —— 构建脚本
# 明确知道自己在做哪端，不该靠推断。
export TRANSLATOR_PLATFORM="${TRANSLATOR_PLATFORM:-android}"

# ── 定位 NDK ──────────────────────────────────────────
find_ndk() {
  # 1) 环境变量优先
  if [[ -n "${ANDROID_NDK_HOME:-}" && -d "$ANDROID_NDK_HOME" ]]; then
    echo "$ANDROID_NDK_HOME"; return 0
  fi
  if [[ -n "${NDK_HOME:-}" && -d "$NDK_HOME" ]]; then
    echo "$NDK_HOME"; return 0
  fi
  # 2) 常见 SDK 位置（本机构建机：D:/Android/Sdk；其次标准位置）
  local candidates=(
    "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}"
    "D:/Android/Sdk" "$LOCALAPPDATA/Android/Sdk"
    "C:/Program Files (x86)/Android/android-sdk"
  )
  local sdk ndk_root latest
  for sdk in "${candidates[@]}"; do
    ndk_root="$sdk/ndk"
    if [[ -d "$ndk_root" ]]; then
      latest="$(ls -1 "$ndk_root" 2>/dev/null | sort -V | tail -1)"
      if [[ -n "$latest" && -d "$ndk_root/$latest" ]]; then
        echo "$ndk_root/$latest"; return 0
      fi
    fi
  done
  # 3) 独立安装的 NDK（非 side-by-side）
  if [[ -d "C:/Program Files (x86)/Android/AndroidNDK/android-ndk-r27c" ]]; then
    echo "C:/Program Files (x86)/Android/AndroidNDK/android-ndk-r27c"; return 0
  fi
  return 1
}

if ! NDK="$(find_ndk)"; then
  cat >&2 <<'EOF'
[FAIL] 未找到 NDK。

安装方式（二选一）：
  1) Android Studio → Settings → Android SDK → SDK Tools
     → 勾选 "NDK (Side by side)"
  2) 命令行：
     android --sdk=$ANDROID_HOME sdk install "ndk;30.0.16248370"

若已装但非默认路径，设环境变量：
  export ANDROID_NDK_HOME=/path/to/ndk/xx.x.xxxxxx
EOF
  exit 1
fi

echo "[INFO] NDK: $NDK"

# ── 配置 linker ───────────────────────────────────────
# NDK r23+ 移除了独立 GCC，linker 在 toolchains/llvm/prebuilt/
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) HOST_TAG="windows-x86_64" ;;
  Darwin)                 HOST_TAG="darwin-x86_64" ;;
  *)                      HOST_TAG="linux-x86_64" ;;
esac

TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
if [[ ! -d "$TOOLCHAIN" ]]; then
  echo "[FAIL] NDK 工具链目录不存在: $TOOLCHAIN" >&2
  echo "       你的 NDK 可能装的是 linux 版（无法在 Windows 使用）" >&2
  exit 1
fi

TARGET="aarch64-linux-android"
# API 24 = Android 7.0，足够 AudioPlaybackCapture（API 29）+ 留余量
API=24

# NDK 对每个 API 级别提供两份：
#   无扩展名  = Unix shell 脚本，Windows 无法直接 exec（os error 193）
#   .cmd      = Windows 批处理包装器 ← Rust 必须用这个
# 这是 Windows 上 Rust + NDK 的经典坑，勿改回无扩展名版本。
LINKER="$TOOLCHAIN/bin/${TARGET}${API}-clang.cmd"

if [[ ! -f "$LINKER" ]]; then
  # 某些 NDK 版本可能只提供 Unix 版，给出明确诊断
  if [[ -f "$TOOLCHAIN/bin/${TARGET}${API}-clang" ]]; then
    echo "[FAIL] 只找到 Unix 版 linker（无 .cmd 包装器）。" >&2
    echo "       Windows 上 Rust 无法直接执行 Unix shell 脚本。" >&2
    echo "       请安装 NDK r26+（Windows 版），而非 linux 版。" >&2
  else
    echo "[FAIL] linker 不存在: $LINKER" >&2
  fi
  exit 1
fi

# ── 交叉编译器 ────────────────────────────────────────
# ⚠️ 必须用 **target 专属** 变量（CC_aarch64_linux_android），绝不能
# 全局 export CC/CXX：cargo 的 host 侧 build-script（如 bzip2-sys，
# sherpa-onnx-sys 的解包依赖）也读 CC —— 全局设置会让 host C 代码
# 被 NDK 的 aarch64 clang 编译 → 找不到 windows.h 直接失败（2026-10-26 实测）。
export CC_aarch64_linux_android="$LINKER"
export CXX_aarch64_linux_android="$TOOLCHAIN/bin/${TARGET}${API}-clang++.cmd"
export AR_aarch64_linux_android="$TOOLCHAIN/bin/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$LINKER"
# ⚠️ ANDROID_NDK（不是 ANDROID_NDK_HOME）：core/build.rs 用它定位
#    NDK 的 sysroot 里的 libc++_static.a（链接 Bergamot 全树需要）。
export ANDROID_NDK="$NDK"

# ── feature 选择 ──────────────────────────────────────
# 完整引擎：sherpa shared（Android 专用）+ Kotlin 注入采集
#           + Bergamot 离线翻译（2026-10-28 接入）。
#
# ⚠️ mt-bergamot 依赖 arm64 版 Bergamot 静态库，
#    由 build_bergamot_android.sh 交叉编译产出。
#    没开这个 feature 时 Rust 侧的 build_mt_backend 会退化成 NullMt
#    —— 表现为「能识别但没有译文」（2026-10-28 用户实测反馈）。
FEATURES="capture-android,asr-sherpa-android,mt-bergamot"
if [[ "$MOCK_ONLY" == "1" ]]; then
  FEATURES="capture-android"
  echo "[INFO] --mock：仅采集契约，不链 sherpa（调试壳）"
fi

# ── Bergamot（翻译引擎）交叉编译 ────────────────────────
# 产物：third_party/bergamot-translator/build-android/
#   ├─ 各个 .a（CMake/Ninja 产出）
#   └─ bergamot_libs.txt（链接清单，build.rs 读它）
#
# 已有产物则跳过（增量构建友好）；用 --rebuild-bergamot 强制重来。
BERGAMOT_BUILD_DIR="$REPO/translator_core/third_party/bergamot-translator/build-android"
BERGAMOT_LIST="$BERGAMOT_BUILD_DIR/bergamot_libs.txt"
if [[ "$MOCK_ONLY" != "1" ]]; then
  if [[ ! -f "$BERGAMOT_LIST" || "${1:-}" == "--rebuild-bergamot" ]]; then
    echo "[INFO] 交叉编译 Bergamot（arm64）……"
    bash "$SCRIPT_DIR/build_bergamot_android.sh" || {
      cat >&2 <<EOF
[FAIL] Bergamot 交叉编译失败 —— Android 端将没有翻译能力。
       可先用 --mock 或临时去掉 mt-bergamot feature 继续其它工作。
EOF
      exit 1
    }
    # 构建目录可能因生成器变更而带后缀（build-android-ninja），
    # 故从脚本输出里取实际目录。
    #
    # ⚠️ 不能写成 `ls ... | head -1`：脚本开了 `set -o pipefail`，
    #    head 读够就退出 → ls 收到 SIGPIPE → 退出码 141/2 →
    #    `set -e` 直接把整个脚本中断（表现为"构建到一半就没了"）。
    #
    # ⚠️ 也不能用 glob 展开（`for d in "$REPO"/.../build-android*`）：
    #    路径含中文/空格时，glob 若无匹配就原样保留字面量，
    #    `[ -d "$d" ]` 恒为假 → 找不到目录。
    #    用「父目录已知 + 逐个子项判断」的方式，不依赖 glob。
    BERGAMOT_PARENT="$REPO/third_party/bergamot-translator"
    BERGAMOT_BUILD_DIR=""
    if [ -d "$BERGAMOT_PARENT" ]; then
      for d in "$BERGAMOT_PARENT"/*; do
        case "$(basename "$d")" in
          build-android*)
            if [ -d "$d" ]; then BERGAMOT_BUILD_DIR="$d"; break; fi
            ;;
        esac
      done
    fi
    if [[ -z "$BERGAMOT_BUILD_DIR" ]]; then
      echo "[FAIL] 找不到 Bergamot 构建目录（$BERGAMOT_PARENT/build-android*）" >&2
      exit 1
    fi
    BERGAMOT_LIST="$BERGAMOT_BUILD_DIR/bergamot_libs.txt"
    # 生成链接清单（build.rs 按它翻译成 rustc-link-* 指令）
    python "$SCRIPT_DIR/gen_bergamot_libs.py" "$BERGAMOT_BUILD_DIR" "$BERGAMOT_LIST" \
      || { echo "[FAIL] 生成 Bergamot 链接清单失败" >&2; exit 1; }
  else
    echo "[INFO] 复用已有 Bergamot 产物（--rebuild-bergamot 可强制重建）"
  fi
  export BERGAMOT_LIBS_LIST="$BERGAMOT_LIST"
  export BERGAMOT_SRC_DIR="$REPO/third_party/bergamot-translator"
fi

# sherpa shared 链接目录（sys crate 用它跳过下载；运行期 rpath=$ORIGIN 解析）
SHERPA_LIB_DIR="$SHERPA_ANDROID/jniLibs/arm64-v8a"
if [[ "$MOCK_ONLY" != "1" ]]; then
  if [[ ! -d "$SHERPA_LIB_DIR" ]]; then
    cat >&2 <<EOF
[FAIL] 未找到 sherpa-onnx Android 包：$SHERPA_LIB_DIR
先运行：bash "$SCRIPT_DIR/fetch_sherpa_android.sh"
EOF
    exit 1
  fi
  export SHERPA_ONNX_LIB_DIR="$SHERPA_LIB_DIR"
fi

# 并行度：容器内可用核数会少，留 1 核给系统
CORES="$( (nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4) )"
JOBS=$(( CORES > 1 ? CORES - 1 : 1 ))

# ── 构建 ──────────────────────────────────────────────
mkdir -p "$JNI_LIBS/arm64-v8a"

echo "[INFO] 构建 profile=$PROFILE  jobs=$JOBS  features=$FEATURES"
if [[ "$DO_CLEAN" == "1" ]]; then
  ( cd "$CORE" && cargo clean --target "$TARGET" )
fi
( cd "$CORE" && cargo build --target "$TARGET" --features "$FEATURES" $CARGO_FLAGS -j "$JOBS" )

# ── 收集产物 ──────────────────────────────────────────
SO="$CORE/target/$TARGET/$PROFILE/libtranslator_core.so"
if [[ ! -f "$SO" ]]; then
  echo "[FAIL] 未找到产物: $SO" >&2
  exit 1
fi

cp -f "$SO" "$JNI_LIBS/arm64-v8a/libtranslator_core.so"
STRIP_BIN="$TOOLCHAIN/bin/llvm-strip"
if [[ -f "$STRIP_BIN" ]]; then
  "$STRIP_BIN" --strip-unneeded "$JNI_LIBS/arm64-v8a/libtranslator_core.so" 2>/dev/null || true
fi

# sherpa 运行库随 APK 分发（sys crate 的链接期 rpath 指向构建机目录，
# 真机解析靠 $ORIGIN —— 即同目录，必须把两库并排放进 jniLibs）
if [[ "$MOCK_ONLY" != "1" ]]; then
  for lib in libsherpa-onnx-c-api.so libonnxruntime.so; do
    if [[ -f "$SHERPA_LIB_DIR/$lib" ]]; then
      cp -f "$SHERPA_LIB_DIR/$lib" "$JNI_LIBS/arm64-v8a/$lib"
      echo "[INFO] 已拷贝 $lib"
    else
      echo "[FAIL] sherpa 包缺少 $lib" >&2
      exit 1
    fi
  done
fi

SIZE="$(du -h "$JNI_LIBS/arm64-v8a/libtranslator_core.so" | cut -f1)"
echo "[PASS] 已生成 $JNI_LIBS/arm64-v8a/libtranslator_core.so ($SIZE)"

# ── 校验（ABI + 链接完整性）────────────────────────────
echo ""
echo "[INFO] 动态依赖校验"
NEEDED="$( "$TOOLCHAIN/bin/llvm-readelf" -d "$JNI_LIBS/arm64-v8a/libtranslator_core.so" \
  | grep NEEDED || true )"
echo "$NEEDED"

if [[ "$MOCK_ONLY" != "1" ]]; then
  # 门禁 1：必须真的链上了 sherpa（否则运行期找不到符号）
  if ! echo "$NEEDED" | grep -q "libsherpa-onnx-c-api.so"; then
    echo "[FAIL] libtranslator_core.so 未依赖 libsherpa-onnx-c-api.so —— 链接没生效" >&2
    exit 1
  fi
  # 门禁 2：必须带真实 ASR 后端符号
  # sherpa-onnx C API 的符号前缀是 `SherpaOnnx*`（大写，非 sherpa_onnx_）。
  # shared 模式下本库对它是 UND 引用（运行期由 libsherpa-onnx-c-api.so 解析）。
  if ! "$TOOLCHAIN/bin/llvm-readelf" --dyn-syms "$JNI_LIBS/arm64-v8a/libtranslator_core.so" \
      | grep -q "SherpaOnnx"; then
    echo "[FAIL] 产物里没有 SherpaOnnx* 符号 —— 疑似 mock 壳" >&2
    exit 1
  fi
  # 门禁 3：禁止静态 bionic（crt-static 残留检查 —— 阶段 0 的验收项）
  if ! echo "$NEEDED" | grep -q "libc.so"; then
    echo "[FAIL] 产物不依赖动态 libc.so —— 疑似仍带 crt-static，请查 .cargo/config.toml" >&2
    exit 1
  fi
  echo "[PASS] 链接校验通过（sherpa 已链入、动态 bionic、含 ASR 符号）"
fi

echo ""
echo "下一步：cd ui/android && gradlew.bat assembleDebug"
