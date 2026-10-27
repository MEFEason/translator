#!/usr/bin/env bash
# 一键构建（Windows 版）+ 发布到 Windows/outputs/，旧产物自动备份到
# Windows/last_good/。
#
# 目录约定（全部在 Translator/ 一个文件夹内）：
#   translator_core/        源码（公共）
#   Windows/                本脚本 + 全部 Windows 产物与安装器
#     scripts/              package.sh 及 Windows 打包/签名脚本
#     outputs/              当前产物（交付用这个）
#     last_good/            上一次的产物（回滚用）
#     dist/                 各版本安装包
#     src/setup.iss         安装器脚本
#   Android/                Android 专属脚本与产物
#
# 用法：
#   bash Windows/scripts/package.sh              # 完整构建 + 发布
#   SKIP_BUILD=1 bash Windows/scripts/package.sh # 只重新打包（复用已构建的）
#
# 安全性：新产物先在 staging 目录组装并**验证通过**，之后才置换
# outputs / last_good。这样构建失败绝不会毁掉上一次的好产物。
# 发布用「目录 mv 置换」实现，全程零 rm -rf（沙箱拦批量删除、
# 且 mv 原子瞬时，不会留下 cp 覆盖式的混入残留）。

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # Windows/scripts/
WIN_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"                      # Windows/
ROOT="$(cd "$WIN_DIR/.." && pwd)"                            # Translator/
REPO="$ROOT/translator_core"                                 # translator_core/
OUT="$WIN_DIR/outputs"
LAST="$WIN_DIR/last_good"
# staging 带时间后缀：每轮新建、永不删除，避免沙箱拦截批量 rm，
# 也杜绝「旧文件靠 cp 覆盖、漏网残留混进产物」的隐患。
# 被置换下来的旧 last_good 移入 Windows/.trash/（可手动清空）。
STAGING="$WIN_DIR/.staging-$(date +%Y%m%d%H%M%S)"
TRASH="$WIN_DIR/.trash"
CORE="$REPO/core"
UI="$REPO/ui"

export PATH="$HOME/.cargo/bin:$PATH"
export PUB_HOSTED_URL="https://pub.flutter-io.cn"
export FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn"

# ★ 增量编译（省时间）★
# 1) Rust：release 模式默认关增量（CARGO_INCREMENTAL=0），打开后重编只碰
#    改动的 crate，不动的走缓存。
# 2) Rust 源码未变则跳过 cargo build：只改 UI/脚本时不必等 Rust 全树重编
#    （实测全量 5-10 分钟，跳过只要几十秒）。
#    用 target/release/translator_core.dll 的 mtime 与 core/src 下最新
#    源文件比较，同时把版本串注入放进去 —— 版本变化也要重编。
#    SKIP_BUILD=1 仍然最高优先，完全跳过构建阶段。
PYTHON_BIN="C:/Users/Admin/.workbuddy/binaries/python/versions/3.13.12/python.exe"
export CARGO_INCREMENTAL=1
RUST_STAMP="$CORE/.rust-build-stamp"
# 是否需要重编 Rust 核心。
# 返回 0 = 需要重编；返回 1 = 可跳过（复用 target/release）。
#
# 判据（两条同时满足才跳过）：
#   1. 版本戳匹配：`semver|channel` 未变
#      （**不含 build_time** —— 它每次都变，含了就永远重编。
#        时间戳的新旧由「从 DLL 反读真实版本串」兜底，见下。）
#   2. core/src、csrc、build.rs 没有比 DLL 更新的文件
#
# ⚠️ 跳过后 .version.ini 必须用 **DLL 里真实的版本串**（反读），
#    否则安装目录名（脚本算的新时间）与关于页（DLL 里的旧时间）不一致。
# 从已编译的 DLL 里反读真实版本串（跳过 Rust 编译时用）。
# DLL 里嵌着 build.rs 生成的 `{channel}-v{semver}-build{YYYY}.{MMDD}.{HH}.{mm}`，
# 用 Python 正则抠出来，保证「关于页 / 安装目录名 / version.ini」三处一致。
read_version_from_dll() {
  "$PYTHON_BIN" -c "
import re,sys
d=open(sys.argv[1],'rb').read()
m=re.search(rb'([a-z]+-v\d+\.\d+\.\d+-build\d{4}\.\d{4}\.\d{2}\.\d{2})',d)
print(m.group(1).decode() if m else '')
" "$CORE/target/release/translator_core.dll"
}

rust_needs_rebuild() {
  [ "${SKIP_BUILD:-0}" = "1" ] && return 1   # SKIP_BUILD 由外层处理
  [ -f "$CORE/target/release/translator_core.dll" ] || return 0
  [ -f "$RUST_STAMP" ] || return 0
  [ "$(cat "$RUST_STAMP" 2>/dev/null)" = "$TRANSLATOR_SEMVER|$TRANSLATOR_CHANNEL" ] || return 0
  local newest
  newest=$(find "$CORE/src" "$CORE/csrc" "$CORE/build.rs" -type f -newer \
           "$CORE/target/release/translator_core.dll" 2>/dev/null | head -1)
  [ -n "$newest" ] && return 0

  # ⚠️ **DLL 内嵌版本必须与本次请求一致**，否则必须重编。
  #
  # 踩过的坑：手工跑 `cargo build --release`（不带 TRANSLATOR_SEMVER /
  # TRANSLATOR_CHANNEL）会把 DLL 里的版本串写成 `snapshot-v0.0.0-build…`，
  # 而版本戳文件仍是旧的 → 源码时间戳也没变 → 脚本误判「可跳过编译」，
  # 于是把版本号错误的 DLL 装进了安装包。
  #
  # 所以这里**以 DLL 里反读到的真实版本串为准**，而不是信任戳文件。
  local dll_ver
  dll_ver="$(read_version_from_dll)"
  case "$dll_ver" in
    "$TRANSLATOR_CHANNEL-v$TRANSLATOR_SEMVER-build"*) return 1 ;;  # 一致 → 可跳过
    *) return 0 ;;   # 不一致或读不到 → 必须重编
  esac
}


# ── 版本号注入 ──
# 版本串形如 `beta-v0.9.1-build2026.1022.15.16`（测试版）
#              `release-v1.0.0-build2026.1022.15.16`（正式版）
#   channel —— 版本渠道（snapshot/alpha/beta/pre/release/stable/current）
#   semver  —— 语义版本 v{主}.{次}.{修订}（TRANSLATOR_SEMVER 注入）
#   build   —— 构建号 build + YYYY.MMDD.HH.mm
#
# 语义版本递进规则（用户规定）：
#   - 正式版大迭代 → 更新全部数字：v1.0.0、v2.0.0
#   - 测试版     → 只改后面的数字，前面依旧为 0：
#                  0.9.1 → 0.9.8 → 0.9.12 → 0.9.45 → 0.10.56
#   - 上一级有变动，下级归零（版本号规范通用准则）
#
# 本地时间由 `date` 提供（构建机本地时区）；Rust 的 std 只有 UTC，
# 拿不到本地时间。
#
# 常用组合：
#   TRANSLATOR_SEMVER=0.9.1 TRANSLATOR_CHANNEL=beta   bash scripts/package.sh   # 测试版
#   TRANSLATOR_SEMVER=1.0.0 TRANSLATOR_CHANNEL=release bash scripts/package.sh # 正式版
#
# ⚠️ 时间**只取一次**，脚本全程复用：
#   开头算出 → 同时给 Rust（TRANSLATOR_BUILD_TIME）与后面的 version.ini。
#   若在组装阶段再取一次 `date`，Rust 编译耗时（分钟级）会导致
#   「关于页版本」与「安装目录名」相差几分钟，对不上。
export TRANSLATOR_BUILD_TIME="$(date '+%Y-%m-%d %H:%M:%S')"
export TRANSLATOR_CHANNEL="${TRANSLATOR_CHANNEL:-snapshot}"
export TRANSLATOR_SEMVER="${TRANSLATOR_SEMVER:-0.0.0}"
# 与 TRANSLATOR_BUILD_TIME 同源，拆成数字便于拼版本串
read -r V_YYYY V_MM V_DD V_HH V_MI _ <<< "$(date '+%Y %m %d %H %M %S')"
V_TIME="$V_YYYY.$V_MM$V_DD.$V_HH.$V_MI"
VERSION_FULL="$TRANSLATOR_CHANNEL-v$TRANSLATOR_SEMVER-build$V_TIME"
# PE 版本资源必须**四段纯数字**（不能带横杠、不能带字母）。
# 语义版本前三段 + 月日时分压一段：v0.9.1 + 20261022.1516 不可行（超 16 位），
# 故用「主.次.修订.月日时分」= 0.9.1.10221516 仍超限
# → 退而用「主.次.修订.月日」= 0.9.1.1022（同日重复构建以完整串区分）
VERSION_NUM="$TRANSLATOR_SEMVER.$V_MM$V_DD"

log()  { echo "[$(date +%H:%M:%S)] $*"; }
die()  { echo "[FAIL] $*" >&2; exit 1; }

# ── 0. 前置检查 ────────────────────────────────────────
command -v cargo >/dev/null || die "找不到 cargo"
[ -x "D:/flutter/flutter/bin/flutter" ] || log "警告：未在 D:/flutter 找到 flutter"

# ★ 去重代理环境变量（Windows 特有，必做）
#
# Windows 的环境块**大小写不敏感**，若同时存在 HTTPS_PROXY 与 https_proxy
# （代理软件常成对写入），CreateProcess 构造环境块时必然失败 →
# cargo 启动任何 build script 都报
#   could not execute process build-script-build (never executed)
#   拒绝访问。 (os error 5)
# 表现为「随机某个 crate 构建失败」，重试换另一个 crate，很有迷惑性。
# 同源症状：PowerShell 的 Get-ChildItem Env: 直接抛
#   "An item with the same key has already been added"
# 保留大写惯例版本，去掉小写重复项。
unset http_proxy https_proxy HTTP_PROXY_HTTPS_PROXY 2>/dev/null || true

# ── 0.5 原生后端的构建环境 ─────────────────────────────
# 三个 feature 一起开：桌面采集 + 真实 ASR（sherpa）+ 真实 MT（Bergamot）。
FEATURES="capture-desktop,mt-bergamot,asr-sherpa"

# Bergamot 静态库（已由 scripts/build_bergamot.py 编好）
BERGAMOT_LIB="$REPO/third_party/bergamot-translator/build/src/translator"
[ -f "$BERGAMOT_LIB/bergamot-translator.lib" ] \
  || die "缺 Bergamot 静态库，先跑：python Windows/scripts/build_bergamot.py"
export BERGAMOT_PREBUILT="$(cygpath -w "$BERGAMOT_LIB" 2>/dev/null || echo "$BERGAMOT_LIB")"
export BERGAMOT_SRC_DIR="$(cygpath -w "$REPO/third_party/bergamot-translator" 2>/dev/null || echo "$REPO/third_party/bergamot-translator")"

# sherpa-onnx 用 **static** 模式：onnxruntime 整份静态编入 exe
# （官方 static-MT 包的 onnxruntime.lib 是 916 MB 真静态库）。
# → 磁盘上**不需要**任何 ORT / sherpa DLL。
#
# 这不只是省几个文件：装一个名为 `onnxruntime.dll` 的新文件会触发
# 360 的「修改系统 DLL」报毒（Inno 进程无论 MoveFile 还是 FileCopy
# 都会被拦），static 模式让这个文件根本不存在。
SHERPA_LIB_DIR="$REPO/third_party/sherpa-prebuilt/sherpa-onnx-v1.13.6-win-x64-static-MT-Release-lib/lib"
[ -d "$SHERPA_LIB_DIR" ] || die "缺 sherpa static-MT 库目录（见 third_party/sherpa-prebuilt/README）"
export SHERPA_ONNX_LIB_DIR="$(cygpath -w "$SHERPA_LIB_DIR" 2>/dev/null || echo "$SHERPA_LIB_DIR")"

# ── 1. 构建 ────────────────────────────────────────────
if [ "${SKIP_BUILD:-0}" = "1" ]; then
  log "1-2/5 跳过构建（SKIP_BUILD=1）"
elif rust_needs_rebuild; then
  log "1/5 构建 Rust 核心（release + $FEATURES，增量）…"
  ( cd "$CORE" && cargo build --release --features "$FEATURES" ) \
    || die "Rust 构建失败"
  cp -f "$CORE/target/release/translator_core.dll" "$UI/windows/rust/translator_core.dll" \
    || die "DLL 拷贝失败"
  printf '%s' "$TRANSLATOR_SEMVER|$TRANSLATOR_CHANNEL" > "$RUST_STAMP"
else
  log "  Rust 源码未变、版本一致 → 跳过编译（CARGO_INCREMENTAL 已启用）"
  DLL_VER="$(read_version_from_dll)"
  if [ -n "$DLL_VER" ]; then
    # 用 DLL 里的真实版本串，保证三处一致
    log "  复用 DLL 内嵌版本: $DLL_VER"
    VERSION_FULL="$DLL_VER"
    VERSION_NUM="$TRANSLATOR_SEMVER.$V_MM$V_DD"
  fi
  cp -f "$CORE/target/release/translator_core.dll" "$UI/windows/rust/translator_core.dll" \
    || die "DLL 拷贝失败"
fi
if [ "${SKIP_BUILD:-0}" != "1" ]; then
  log "2/5 构建 Flutter 外壳（release）…"
  # 两个 Windows 特有的坑，都会让构建在 INSTALL 阶段失败：
  #
  # (1) `build/native_assets/windows/` 不存在
  #     Flutter 的 windows CMake 模板**无条件**生成
  #       file(INSTALL ... FILES ".../build/native_assets/windows/")
  #     而该目录只在 pubspec 声明了 native assets 依赖时才会被创建。
  #     本项目没声明 → 目录不存在 → install 报
  #       file INSTALL cannot find .../native_assets/windows
  #     建个空目录即可（install 空目录合法，什么都不会拷）。
  #     ⚠️ 不要为此删整个 build/ —— flutter_assets 等重建很慢，
  #        且删不掉问题根源。
  #
  # (2) 插件 symlink 退化成空目录
  #     Flutter 用 dart:io 建 symlink，在"非管理员 + 未开开发者模式"下
  #     会静默失败，之后报
  #       add_subdirectory .plugin_symlinks/jni/windows 不存在
  #     改用 NTFS junction（不需要管理员权限）。
  mkdir -p "$UI/build/native_assets/windows"
  if [ -f "$SCRIPT_DIR/windows_plugin_junctions.ps1" ]; then
    SYMLINKS="$UI/windows/flutter/ephemeral/.plugin_symlinks"
    NEED_FIX=0
    [ -d "$SYMLINKS" ] || NEED_FIX=1
    for d in "$SYMLINKS"/*/; do
      [ -d "$d" ] && [ -z "$(ls -A "$d" 2>/dev/null)" ] && NEED_FIX=1
    done
    if [ "$NEED_FIX" = "1" ]; then
      log "  重建 Windows 插件联接（junction）…"
      powershell -NoProfile -ExecutionPolicy Bypass \
        -File "$SCRIPT_DIR/windows_plugin_junctions.ps1" >/dev/null 2>&1 \
        || log "  警告：junction 重建失败，构建若报插件错误请手动运行该脚本"
    fi
  fi

  ( cd "$UI" && "D:/flutter/flutter/bin/flutter" build windows --release ) \
    || die "Flutter 构建失败"
else
  log "1-2/5 跳过构建（SKIP_BUILD=1）"
fi

# ── 2. 组装到 staging ──────────────────────────────────
log "3/5 组装到 staging…"
RELEASE="$UI/build/windows/x64/runner/Release"
[ -f "$RELEASE/translator.exe" ] || die "找不到构建产物 $RELEASE/translator.exe"
# staging 目录名带时间戳、每轮全新创建，无需删除旧 staging

# ── 5. 编译并签名安装器（读**刚发布**的 outputs）──────────────────
# 安装器内含静默装证书的注册表段（Blob 与 codesign.pfx 绑定）。
# 若 cert_tools.ps1 同步过 setup.iss，这里编译出的就是最新证书版本。
ISCC=""
for c in "/c/Users/Admin/WorkBuddy/代码开发/tools/innosetup/ISCC.exe" \
         "/c/Users/Admin/WorkBuddy/代码开发/tools/innosetup/app/ISCC.exe" \
         "/c/Program Files (x86)/Inno Setup 6/ISCC.exe"; do
  [ -f "$c" ] && ISCC="$c" && break
done
# ★ 版本号必须在编译安装器**之前**落盘 ★
# setup.iss 用 ISPP ReadIni 读 $WIN_DIR/.version.ini 拿版本串。
# 若等 staging 组装完再写，安装器打包的会是上一轮的旧版本号 ——
# 「安装器名是新版本、装出来的目录名是旧版本」这种极难察觉的错。
# （staging/version.ini 仍在组装阶段写，随产物发布。）
for vf in "$WIN_DIR/.version.ini"; do
  printf '[version]\nfull=%s\nchannel=%s\nsemver=%s\nnum=%s\n' \
    "$VERSION_FULL" "$TRANSLATOR_CHANNEL" "$TRANSLATOR_SEMVER" "$VERSION_NUM" > "$vf" \
    || die "写版本文件失败: $vf"
done
log "  版本号: $VERSION_FULL  (PE 资源用 $VERSION_NUM)"

# ⚠️ 安装器的「编译 + 签名」已**移到 5/5 发布之后**（见脚本尾部）。
#   原因：setup.iss 用 `Source "..\outputs\..."` 取二进制，必须在
#   发布 outputs **之后**才拿得到本轮新产物；若在发布前编译，安装器
#   打进的是上一轮的旧 translator_core.dll / app.so —— 版本号标签是
#   新的、里面二进制却是旧的（「关于页版本」与真实二进制不符，极难
#   察觉）。踩过。这里只保留 ISCC 探测与版本号落盘。
mkdir -p "$STAGING"
# ★ 清掉 Flutter Release 目录里的**旧版** ORT / sherpa DLL。
# 早期版本用 shared 模式时会往这里 cp DLL；现在改 static 模式，
# 这些文件不该再存在，但会残留在 build 目录里被一起拷进产物。
for stale in onnxruntime.dll onnxruntime_providers_shared.dll \
             sherpa-onnx-c-api.dll sherpa-onnx-cxx-api.dll ort_runtime.dll; do
  rm -f "$RELEASE/$stale"
done
cp -f "$RELEASE"/translator.exe "$RELEASE"/translator_core.dll \
      "$RELEASE"/flutter_windows.dll "$STAGING"/ || die "拷贝可执行文件失败"
cp -rf "$RELEASE/data" "$STAGING"/ || die "拷贝 data/ 失败"

# ★★ 收集 Flutter 插件的原生 DLL ★★
#
# Flutter 的每个带原生实现的插件都会在 Release/ 下生成一个
# `<plugin>_plugin.dll`，它们是**运行时必需**的依赖：
# 缺一个 → 启动即弹「由于找不到 xxx.dll，无法继续执行代码」。
#
# 踩过的坑：早期这里只硬编码拷 3 个文件（exe / translator_core.dll /
# flutter_windows.dll）。加了 window_manager + tray_manager 后，
# 产物漏掉 window_manager_plugin.dll / screen_retriever_windows_plugin.dll /
# cnativeapi.dll → 安装后 exe 直接起不来。
#
# 所以：**不要枚举插件**，把 Release/ 下所有 DLL 一并带走
# （除已知的 ORT/sherpa 黑名单外）。新增插件自动生效。
log "  收集插件原生 DLL…"
plugin_dlls=0
for d in "$RELEASE"/*.dll; do
  [ -f "$d" ] || continue
  base="$(basename "$d")"
  case "$base" in
    translator_core.dll|flutter_windows.dll) continue ;;  # 已拷
    onnxruntime*|sherpa-onnx-*|ort_runtime.dll) continue ;; # ORT 黑名单
  esac
  cp -f "$d" "$STAGING"/ || die "拷贝插件 DLL 失败: $base"
  log "    + $base"
  plugin_dlls=$((plugin_dlls+1))
done
log "  已收集 $plugin_dlls 个插件 DLL"

# ★★ static 模式：磁盘上**不允许**出现任何 ORT / sherpa DLL ★★
#
# 1) sherpa-onnx 用 static 模式，onnxruntime 整份静态编入 exe。
# 2) 更重要：装一个名为 `onnxruntime.dll` 的新文件会触发 360 的
#    「修改系统 DLL」报毒 —— Inno 进程无论 MoveFile / FileCopy /
#    cmd copy 都一律「Access is denied」（只有完全精确的
#    `onnxruntime.dll` 这个文件名被拦，其它 .dll 名都正常）。
# 3) 还踩过：Windows 11 24H2 的 System32 自带 onnxruntime.dll
#    （v1.17，Microsoft AI 组件用），而 **System32 的搜索优先级高于 PATH**
#    → 装不进自己的 1.27 就会加载到系统的 1.17（API 只到 17，
#    sherpa 需要 27）→ 加载 sherpa 时崩在 DLL 内部。
#    static 模式让这个文件根本不存在，问题从根上消失。
for bad in onnxruntime.dll onnxruntime_providers_shared.dll \
           sherpa-onnx-c-api.dll sherpa-onnx-cxx-api.dll; do
  if [ -f "$STAGING/$bad" ]; then
    die "产物里出现了 $bad —— static 模式不应分发 ORT DLL"
  fi
done
log "  OK 产物零 ORT DLL（static 模式）"

# ★ 模型权重（ASR zipformer + MT Bergamot，共 ~94 MB）
# 真实后端启动必需；mock 模式下可不打包（SKIP_MODELS=1）。
if [ "${SKIP_MODELS:-0}" != "1" ] && [ -d "$REPO/models" ]; then
  cp -rf "$REPO/models" "$STAGING"/ || die "拷贝模型失败"
  log "  已收录模型：$(du -sh "$STAGING/models" | cut -f1)"
fi

# README 与校验和
# ★ 托盘图标（app_icon.ico）★
# 托盘服务（ui/lib/services/tray_service.dart）从 exe 同目录读这个文件，
# 用 LoadImageW 加载成 HICON 喂给 Shell_NotifyIcon。
# 缺它 → 托盘起不来（日志会打「找不到托盘图标文件」）。
cp -f "$UI/windows/runner/resources/app_icon.ico" "$STAGING"/app_icon.ico \
  || die "拷贝托盘图标失败"

cp -f "$SCRIPT_DIR/README.md" "$STAGING"/README.md 2>/dev/null \
  || log "警告：scripts/README.md 不存在，跳过"

# ★ 许可证文件（FSL-1.1-ALv2 的 Redistribution 条款要求）★
#
# FSL 明确规定：再分发本软件的任何副本/修改/衍生作品时，
# **必须附带条款副本或链接，且不得移除已有的版权声明**。
# 同时本项目把 ONNX Runtime(MIT) / sherpa-onnx(Apache-2.0) /
# Bergamot(MPL-2.0) 等**静态链接**进了 translator_core.dll，
# 静态链接意味着这些组件的许可告知义务直接适用于分发行为 ——
# 所以 LICENSE + NOTICE + 第三方许可原文都必须进产物。
#
# 缺任何一个 → 合规风险，不是可选装饰。
for lf in LICENSE NOTICE; do
  cp -f "$ROOT/$lf" "$STAGING"/"$lf" \
    || die "拷贝 $lf 失败（许可证文件缺失 = 合规风险，禁止发布）"
done
# 第三方许可原文目录
if [ -d "$ROOT/THIRD_PARTY_LICENSES" ]; then
  cp -rf "$ROOT/THIRD_PARTY_LICENSES" "$STAGING"/ \
    || die "拷贝 THIRD_PARTY_LICENSES/ 失败"
else
  die "THIRD_PARTY_LICENSES/ 不存在（静态链接组件的许可告知义务无法满足）"
fi

# ★ 生成安装器许可页 RTF（Inno 的 LicenseFile 只认 RTF）★
# 从根目录 LICENSE 自动生成 → 条款永远与 LICENSE 一致，
# 避免「改了 LICENSE 忘了改许可页」这种不一致（安装器合规要看的正是这一份）。
python - "$ROOT/LICENSE" "$WIN_DIR/src/LICENSE.rtf" <<'PYEOF'
import io, re, sys
src_path, out_path = sys.argv[1], sys.argv[2]
body = re.sub(r'^#{1,6}\s*', '', io.open(src_path, encoding='utf-8').read(), flags=re.M)
body = body.replace('**', '').replace('`', '')
def esc(t):
    out = []
    for ch in t:
        if ch in '\\{}':
            out.append('\\' + ch)
        elif ord(ch) < 128:
            out.append(ch)
        else:
            cp = ord(ch)
            out.append('\\u%d?' % (cp if cp < 32768 else cp - 65536))
    return ''.join(out)
lines = [esc(l.strip()) + '\\par' if l.strip() else '\\par' for l in body.split('\n')]
rtf = ('{\\rtf1\\ansi\\ansicpg1252\\deff0\\deflang1033'
       '{\\fonttbl{\\f0\\fnil\\fcharset0 Segoe UI;}}'
       '\\viewkind4\\uc1\\pard\\f0\\fs18 ' + '\n'.join(lines) + '}\n')
io.open(out_path, 'w', encoding='ascii', newline='\r\n').write(rtf)
PYEOF
[ $? -eq 0 ] || die "生成 LICENSE.rtf 失败（安装器许可页将缺失）"
[ -s "$WIN_DIR/src/LICENSE.rtf" ] || die "LICENSE.rtf 为空（安装器许可页将缺失）"

# ★ 版本号落盘（供 setup.iss 读取）★
#
# 版本号的**唯一来源**是 Rust 侧的 build.rs（TRANSLATOR_CHANNEL + TRANSLATOR_SEMVER +
# TRANSLATOR_BUILD_TIME），这里把同一个值写进 version.ini，安装器用 ISPP 的
# ReadIni 读取 → 安装目录与「关于页」显示的版本号永远一致，
# 不会各写各的而脱节。
#
# 写两处：
#   $WIN_DIR/.version.ini   —— 给 setup.iss 读（**非交付物**，不参与
#                          outputs/last_good 的「验证后才覆盖」交换；
#                          放在这里是为了让安装器在 staging 阶段就能
#                          编译拿到正确版本号）
#   $STAGING/version.ini —— 随产品发布（应用可读取）
#
# 复用脚本开头算好的 VERSION_FULL / VERSION_NUM（不重新取 date）。
# $WIN_DIR/.version.ini 已在「编译安装器之前」写好，这里只写随产物发布的那份。
printf '[version]\nfull=%s\nchannel=%s\nsemver=%s\nnum=%s\n' \
  "$VERSION_FULL" "$TRANSLATOR_CHANNEL" "$TRANSLATOR_SEMVER" "$VERSION_NUM" \
  > "$STAGING/version.ini" || die "写版本文件失败: $STAGING/version.ini"

# ⚠️ SHA256SUMS.txt 不在这里算 —— 签名（3.5 节）会改写 exe/dll 的字节，
# 若在签名前算，清单与最终产物必然对不上（踩过：6 个文件校验 FAILED）。
# 正确位置：签名之后、发布之前（见「3.6 重算 SHA256」）。

# ── 3. 验证 staging（失败则不动 outputs/last_good）─────
log "4/5 验证产物…"
fail=0
for f in translator.exe translator_core.dll flutter_windows.dll app_icon.ico \
         data/app.so data/icudtl.dat version.ini; do
  [ -f "$STAGING/$f" ] || { echo "  ✗ 缺失 $f"; fail=1; }
done
# SHA256SUMS.txt 不在此清单：它在签名之后才生成（3.6 节）
[ "$fail" = "0" ] || die "产物不完整，已中止（outputs 与 last_good 未受影响）"

# ★ 插件原生 DLL 必须在产物里 ★
#
# 漏一个 → 装完双击 exe 弹「由于找不到 xxx.dll，无法继续执行代码」，
# 应用直接起不来（踩过：window_manager / screen_retriever / cnativeapi）。
# 所以这里**按 Flutter 插件清单逐个核对**，而不是看总数 ——
# 数量对但名字错同样会崩。
#
# 插件清单来自 .flutter-plugins-dependencies（flutter pub get 生成）。
PLUGIN_DEPS="$UI/.flutter-plugins-dependencies"
if [ -f "$PLUGIN_DEPS" ]; then
  # 每个插件的 windows 实现名 = <name>_windows_plugin
  # nativeapi / tray_manager 之类提供 windows 目录的插件，产物里是
  # <name>_plugin.dll；按 Release 目录里实际存在的 *_plugin.dll 反查。
  while IFS= read -r dll; do
    [ -f "$STAGING/$dll" ] || { echo "  ✗ 缺少插件 DLL: $dll"; fail=1; }
  done < <(cd "$RELEASE" && ls -1 *.dll 2>/dev/null | grep -E "_plugin\.dll$|^cnativeapi\.dll$")
fi
[ "$fail" = "0" ] || die "产物不完整（插件 DLL 缺失），已中止（outputs 与 last_good 未受影响）"

# SHA256 校验挪到「签名后重算」一节（3.6），此处先只查文件齐全
log "  文件齐全（SHA256 待签名后计算并校验）"

# 验证 DLL 真的导出预期符号（防止把旧 DLL 打包进去）
# bash 的 $STAGING 是 /c/... 形式，Python 需要 Windows 路径 —— cygpath 转换
STAGING_WIN="$(cygpath -w "$STAGING" 2>/dev/null || echo "$STAGING")"
SYMS=$("C:/Users/Admin/.workbuddy/binaries/python/versions/3.13.12/python.exe" -c "
import re,sys
d=open(r'$STAGING_WIN/translator_core.dll','rb').read()
s={x.decode() for x in re.findall(rb'translator_[a-z_]+',d)}-{'translator_core'}
# 完整清单 = Dart 侧 bridge.dart 实际 lookupFunction 的全部符号。
# ⚠️ 之前只查 6 个，漏了 translator_model_import_dir ——
#    结果是「打包通过、门禁通过」，但应用启动即崩
#    （Failed to lookup symbol）。清单必须与 tests/exports.rs 保持一致。
need={'translator_session_create','translator_session_free',
      'translator_session_start','translator_session_stop',
      'translator_session_pause','translator_session_take_events',
      'translator_session_status','translator_session_error_desc',
      'translator_session_partial','translator_session_export',
      'translator_session_capture_capability','translator_session_capture_handle',
      'translator_glossary_load','translator_glossary_reload',
      'translator_model_import_dir','translator_selfcheck_json',
      'translator_transcript_dir','translator_transcript_path',
      'translator_transcript_query','translator_transcript_stats',
      'translator_create_capture','translator_free_capture',
      'translator_start_capture','translator_stop_capture',
      'translator_push_chunk','translator_poll_chunk',
      'translator_dropped_frames','translator_probe_capture',
      'translator_version','translator_is_offline','translator_free_string',
      'translator_capability_desc','translator_chunk_samples',
      'translator_sample_rate','translator_discover_models',
      'translator_probe_system_load','translator_resource_tiers'}
missing=need-s
if missing:
    print('缺少: ' + ','.join(sorted(missing)))
print(f'{len(s)} 个符号')
sys.exit(1 if missing else 0)
" ) || die "DLL 符号不完整（可能打包了旧 DLL）：$SYMS"
log "  DLL $SYMS，关键符号齐全"

# ── 3.2 出厂产物验证门禁（验证通过才允许覆盖 last_good / 发布）──
# 加载真实 translator_core.dll + 真实模型，确认自检 lib/asr/mt/glossary
# 全部 ok（与有无音频设备无关）。验证不过则中止发布，last_good 与
# outputs 一律保持原状，绝不会用坏产物覆盖好产物。
log "  出厂验证（加载真实 DLL + 真实模型自检）…"
if "$PYTHON_BIN" "$SCRIPT_DIR/verify_product.py" "$STAGING_WIN"; then
  log "  ✓ 出厂验证通过"
else
  die "出厂验证未通过 —— 已中止发布（last_good 与 outputs 保持不变）"
fi

# ── 3.5 代码签名（360 QVM 降误报）─────────────────────
# 360 主防对「无签名二进制」命中率高（实测 outputs\translator.exe
# 被 360hips 记录）。版本资源在构建期注入，签名在这里补齐。
# 证书缺失时只警告不阻断 —— 离线环境首次运行需先跑 _mkcert.ps1。
log "  签名产物…"
if [ -f "$SCRIPT_DIR/codesign.pfx" ]; then
  if powershell -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$SCRIPT_DIR/sign_outputs.ps1")" -TargetDir "$(cygpath -w "$STAGING")" 2>&1 | tail -8; then
    log "  签名完成"
  else
    log "  [警告] 签名失败，产物未签名（可能被 360 误报）"
  fi
else
  log "  [跳过] 无 codesign.pfx。运行 Windows/scripts/_renew_cert.ps1 生成证书后重新打包"
fi

# ── 3.6 SHA256SUMS 必须在**签名之后**计算 ──────────────
# 签名会改写 exe/dll 字节；若在签名前算，清单与最终产物必然不一致
# （踩过：outputs 里 6 个文件 sha256sum -c 报 FAILED）。
log "  计算 SHA256（签名后）…"
( cd "$STAGING" && \
  find . -type f ! -name SHA256SUMS.txt -exec sha256sum {} \; > SHA256SUMS.txt ) \
  || die "生成 SHA256SUMS 失败"
( cd "$STAGING" && sha256sum -c SHA256SUMS.txt --quiet ) \
  || die "SHA256 自校验失败（签名后清单与产物不符）"
log "  SHA256 清单已生成并校验通过"

# ── 4. 备份旧产物 → last_good，新产物 → outputs ────────
# ⚠️ 顺序很关键：必须**先发布 outputs，再编译安装器**。
#    setup.iss 从 `..\outputs\` 取文件（Source: "..\outputs\..."），
#    若先编安装器、后发布 outputs，安装器打进去的是**上一轮**的旧产物 ——
#    装完目录名是新版本号，里面的 app.so 却是旧的，极难察觉。
#    （踩过：修了窗口初始化崩溃，装完照旧崩 —— 因为装的是旧 app.so。）
log "5/5 发布（目录 mv 置换，零 rm -rf）…"
mkdir -p "$TRASH"
TS="$(date +%Y%m%d%H%M%S)"
if [ -d "$OUT" ] && [ -n "$(ls -A "$OUT" 2>/dev/null)" ]; then
  # 旧 last_good 移入 .trash 存档，新位置用 mv 原子置换（瞬时、不留残）
  if [ -d "$LAST" ]; then
    mv "$LAST" "$TRASH/last_good-$TS" || die "旧 last_good 移入 .trash 失败"
  elif [ -e "$LAST" ]; then
    rm -f "$LAST"
  fi
  mv "$OUT" "$LAST" || die "备份到 last_good 失败（mv 置换）"
  log "  旧产物已置换为 last_good/"
else
  log "  outputs 为空，无需备份"
fi

# 旧 outputs 同样移入 .trash（不 rm -rf，避免沙箱拦截与半成品覆盖）
[ -d "$OUT" ] && mv "$OUT" "$TRASH/outputs-$TS"
mv "$STAGING" "$OUT" || die "发布到 outputs 失败（mv 置换）"
log "  新产物已就位；置换下来的旧目录在 $TRASH/（可手动清空）"

# ── 5.5 编译并签名安装器（**必须在发布 outputs 之后**）──
# setup.iss 用 `Source "..\outputs\..."` 取二进制 —— 只有 outputs
# 已是本轮新产物，装进安装器的才是新 DLL / 新 app.so。
# （2026-10-25 修复：此前这段写在发布前，导致新安装器打包上一轮旧二进制。）
if [ -n "$ISCC" ]; then
  log "  编译安装器（基于已发布的 outputs）…"
  if "$ISCC" "$(cygpath -w "$WIN_DIR/src/setup.iss")" 2>&1 | tail -2; then
    powershell -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$SCRIPT_DIR/sign_outputs.ps1")" -TargetDir "$(cygpath -w "$WIN_DIR/dist")" 2>&1 | tail -3
    log "  安装器已签名: $WIN_DIR/dist/"
  else
    log "  [警告] 安装器编译失败，跳过（不影响 outputs 产物）"
  fi
else
  log "  [跳过] 未找到 ISCC.exe，未编译安装器"
fi

log ""
log "完成。产物在：$OUT"
log "  可执行文件：$(ls -la "$OUT"/translator.exe | awk '{print $5}') bytes"
log "  总大小：$(du -sh "$OUT" | awk '{print $1}')"
log "  上一版本：$LAST"
log ""
log "运行：双击 $OUT/translator.exe"
