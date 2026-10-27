cmake_minimum_required(VERSION 3.22)
project(translator_native_bridge)

# JNI 桥接层：Kotlin external fun → Rust C ABI
#
# ## 必须链接 Rust 库
# native_bridge.cpp 调用 translator_* 系列符号，不链接 libtranslator_core.so
# 会报 "undefined symbol: translator_create_capture"。
# 该库由 scripts/build_android.sh 预编译到 jniLibs/<abi>/。

find_library(log-lib log)

# ## ABI 探测
# ANDROID_ABI 由 AGP toolchain 在 **ninja 构建阶段**传入，
# 但 CMake configure 阶段可能拿不到（实测踩过 find_library 落空）。
# 故缺失时回退到工程唯一目标 ABI。
if(DEFINED ANDROID_ABI AND NOT ANDROID_ABI STREQUAL "")
    set(TARGET_ABI "${ANDROID_ABI}")
else()
    # 本工程只支持 arm64-v8a（卓易通容器是 arm64；带其它 ABI 会让
    # core + sherpa-onnx + Bergamot 各复制一份，包体暴涨）
    set(TARGET_ABI "arm64-v8a")
    message(WARNING "ANDROID_ABI 未定义，回退到 ${TARGET_ABI}")
endif()

# cpp/ 与 jniLibs/ 同在 src/main/ 下，故只需一层
set(RUST_LIB_DIRS
    "${CMAKE_CURRENT_SOURCE_DIR}/../jniLibs/${TARGET_ABI}"
    "${CMAKE_CURRENT_SOURCE_DIR}/../jniLibs/arm64-v8a"
)

# ⚠️ 2026-10-26 修复：**不能用 find_library**。
# 工程路径含中文（…\代码开发\…）时，CMake 的 find_library 对非 ASCII
# 路径解析失败 —— glob 明明命中了 libtranslator_core.so，
# find_library 却返回 NOTFOUND，configure 直接 FATAL_ERROR。
# 改用 IMPORTED SHARED 显式导入：路径原样传给链接器，不经 find_library。
set(RUST_SO_PATH "")
foreach(dir ${RUST_LIB_DIRS})
    if(EXISTS "${dir}/libtranslator_core.so")
        set(RUST_SO_PATH "${dir}/libtranslator_core.so")
        break()
    endif()
endforeach()

if(NOT RUST_SO_PATH)
    file(GLOB FOUND_SO "${CMAKE_CURRENT_SOURCE_DIR}/../jniLibs/*/libtranslator_core.so")
    message(FATAL_ERROR
        "找不到 libtranslator_core.so。\n"
        "  ABI: ${TARGET_ABI}\n"
        "  已搜索: ${RUST_LIB_DIRS}\n"
        "  glob 命中: ${FOUND_SO}\n"
        "请先运行: bash Android/scripts/build_android.sh --release")
endif()

# ⚠️ ABI 必须匹配：把 arm64 的 .so 链进 armv7 会报
# "is incompatible with armelf_linux_eabi"。
# 本工程只产出 arm64-v8a（卓易通容器是 arm64，带其它 ABI 包体暴涨）。
# 若 Gradle 仍要求构建其它 ABI（如 armeabi-v7a），直接跳过该 ABI 的
# native 构建 —— 那些 ABI 最终也不会进 APK（defaultConfig 已限 abiFilters）。
if(NOT TARGET_ABI STREQUAL "arm64-v8a")
    message(STATUS "跳过 ${TARGET_ABI}：本工程只构建 arm64-v8a")
    return()
endif()

message(STATUS "找到 Rust 核心库: ${RUST_SO_PATH}")

add_library(translator_core SHARED IMPORTED)
set_target_properties(translator_core PROPERTIES
    IMPORTED_LOCATION "${RUST_SO_PATH}"
)

add_library(native_bridge SHARED
    native_bridge.cpp
)

# ⚠️ 两个 .so 都要打进 APK：
# libtranslator_core.so 在 jniLibs（Gradle 打包）
# libnative_bridge.so 在此构建
target_link_libraries(native_bridge
    translator_core
    ${log-lib}
)

set_target_properties(native_bridge PROPERTIES
    CXX_STANDARD 17
    CXX_STANDARD_REQUIRED ON
)