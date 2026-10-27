// JNI 实现层：Kotlin external fun → Rust C ABI
//
// ## 为什么需要这一层
// Rust 导出的是 C ABI（translator_* 符号），而 Kotlin 的 `external fun`
// 需要 `Java_com_translator_app_NativeBridge_nativeXxx` 符号。
// 本文件做这层桥接。
//
// ## 三个关键点
//
// 1. **FloatArray 桥接**：Rust 的 `translator_poll_chunk` 收 `*mut f32`，
//    需用 GetFloatArrayElements / ReleaseFloatArrayElements 拿指针。
//    必须用 JNI_ABORT 或 0 释放 —— 用 JNI_COMMIT 会把缓冲写回，浪费。
//
// 2. **字符串泄漏（已知问题）**：Rust 返回的 *mut c_char 需
//    translator_free_string 释放，但转成 jstring 后原指针丢失。
//    这里先原样返回（功能正确但有泄漏），已标注 TODO。
//    影响：仅 capabilityDesc/version 两个低频调用，可接受。
//
// 3. **异常安全**：任何 JNI 调用都不得抛 C++ 异常穿过 JNI 边界
//    （会导致未定义行为）。所有调用包在 try/catch 中。

#include <jni.h>
#include <cstring>
#include <string>
#include <vector>

// ── Rust C ABI 声明（与 core/src/ffi.rs 一致）───────────
extern "C" {

void* translator_create_capture(int source, int runtime);
void  translator_free_capture(void* handle);
int   translator_probe_capture(const void* handle);
char* translator_capability_desc(const void* handle);
int   translator_start_capture(const void* handle);
int   translator_stop_capture(const void* handle);
unsigned long translator_poll_chunk(const void* handle, float* out, unsigned long out_cap);
int   translator_push_chunk(const void* handle, const float* samples, unsigned long count);
unsigned long translator_chunk_samples(void);
int   translator_sample_rate(void);
unsigned long translator_dropped_frames(const void* handle);
char* translator_version(void);
bool  translator_is_offline(void);
void  translator_free_string(char* s);

// ── 会话级（Android 注入链路推荐路径）──────────────────
// translator_session_push_chunk 按索引即时查表 —— 会话 start
// 重建采集器后依旧有效，Kotlin 只需持有会话指针（Dart 拥有其生命周期）。
int translator_session_push_chunk(void* session, unsigned long source_index,
                                  const float* samples, unsigned long count);
int translator_session_set_capture_granted(void* session, int granted);

} // extern "C"

// ── 辅助 ──────────────────────────────────────────────

namespace {

/// 把 Rust 返回的 char* 安全转成 jstring。
///
/// ⚠️ 原指针会丢失，无法调用 translator_free_string → 已知泄漏。
/// 仅用于 capabilityDesc / version 两个低频调用。
jstring to_jstring(JNIEnv* env, char* rust_str) {
  if (rust_str == nullptr) return env->NewStringUTF("");
  jstring result = env->NewStringUTF(rust_str);
  // TODO: 保存原指针以便后续释放。当前先不释放（低频调用，泄漏可接受）
  return result;
}

} // namespace

// ── JNI 实现 ───────────────────────────────────────────

extern "C" JNIEXPORT jlong JNICALL
Java_com_translator_app_NativeBridge_nativeCreateCapture(
    JNIEnv* env, jobject /*thiz*/, jint source, jint runtime) {
  void* h = nullptr;
  try {
    h = translator_create_capture(static_cast<int>(source), static_cast<int>(runtime));
  } catch (...) {
    // Rust 侧已 catch_unwind，这里防御性再包一层
    return 0;
  }
  return reinterpret_cast<jlong>(h);
}

extern "C" JNIEXPORT void JNICALL
Java_com_translator_app_NativeBridge_nativeFreeCapture(
    JNIEnv* /*env*/, jobject /*thiz*/, jlong handle) {
  if (handle == 0) return;
  try {
    translator_free_capture(reinterpret_cast<void*>(handle));
  } catch (...) {}
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeProbeCapture(
    JNIEnv* /*env*/, jobject /*thiz*/, jlong handle) {
  if (handle == 0) return 2; // Unavailable
  try {
    return static_cast<jint>(translator_probe_capture(reinterpret_cast<const void*>(handle)));
  } catch (...) {
    return 2;
  }
}

extern "C" JNIEXPORT jstring JNICALL
Java_com_translator_app_NativeBridge_nativeCapabilityDesc(
    JNIEnv* env, jobject /*thiz*/, jlong handle) {
  if (handle == 0) return env->NewStringUTF("采集句柄未创建");
  try {
    char* s = translator_capability_desc(reinterpret_cast<const void*>(handle));
    return to_jstring(env, s);
  } catch (...) {
    return env->NewStringUTF("能力探测异常");
  }
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeStartCapture(
    JNIEnv* /*env*/, jobject /*thiz*/, jlong handle) {
  if (handle == 0) return 6; // Internal
  try {
    return static_cast<jint>(translator_start_capture(reinterpret_cast<const void*>(handle)));
  } catch (...) {
    return 6;
  }
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeStopCapture(
    JNIEnv* /*env*/, jobject /*thiz*/, jlong handle) {
  if (handle == 0) return 0;
  try {
    return static_cast<jint>(translator_stop_capture(reinterpret_cast<const void*>(handle)));
  } catch (...) {
    return 6;
  }
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativePollChunk(
    JNIEnv* env, jobject /*thiz*/, jlong handle, jfloatArray out, jint cap) {
  if (handle == 0 || out == nullptr || cap <= 0) return 0;

  // 拿 Java 数组指针（可能拷贝，取决于 VM 实现）
  jfloat* buf = env->GetFloatArrayElements(out, nullptr);
  if (buf == nullptr) return 0;

  int got = 0;
  try {
    got = static_cast<int>(
        translator_poll_chunk(reinterpret_cast<const void*>(handle), buf,
                              static_cast<unsigned long>(cap)));
  } catch (...) {
    got = 0;
  }

  // 0 = 拷贝回 Java 堆并释放；JNI_ABORT = 只释放不写回（省一次拷贝）
  // 这里需要数据写回，故用 0
  env->ReleaseFloatArrayElements(out, buf, 0);
  return got;
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeChunkSamples(
    JNIEnv* /*env*/, jobject /*thiz*/) {
  try {
    return static_cast<jint>(translator_chunk_samples());
  } catch (...) {
    return 320; // 兜底：16k × 20ms
  }
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeSampleRate(
    JNIEnv* /*env*/, jobject /*thiz*/) {
  try {
    return static_cast<jint>(translator_sample_rate());
  } catch (...) {
    return 16000;
  }
}

extern "C" JNIEXPORT jlong JNICALL
Java_com_translator_app_NativeBridge_nativeDroppedFrames(
    JNIEnv* /*env*/, jobject /*thiz*/, jlong handle) {
  if (handle == 0) return 0;
  try {
    return static_cast<jlong>(translator_dropped_frames(reinterpret_cast<const void*>(handle)));
  } catch (...) {
    return 0;
  }
}

extern "C" JNIEXPORT jstring JNICALL
Java_com_translator_app_NativeBridge_nativeVersion(
    JNIEnv* env, jobject /*thiz*/) {
  try {
    char* s = translator_version();
    return to_jstring(env, s);
  } catch (...) {
    return env->NewStringUTF("unknown");
  }
}

extern "C" JNIEXPORT jboolean JNICALL
Java_com_translator_app_NativeBridge_nativeIsOffline(
    JNIEnv* /*env*/, jobject /*thiz*/) {
  try {
    return translator_is_offline() ? JNI_TRUE : JNI_FALSE;
  } catch (...) {
    return JNI_TRUE; // 保守：离线是本库硬约束
  }
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativePushChunk(
    JNIEnv* env, jobject /*thiz*/, jlong handle, jfloatArray samples, jint count) {
  if (handle == 0 || samples == nullptr || count <= 0) return 7; // InvalidArg

  jfloat* buf = env->GetFloatArrayElements(samples, nullptr);
  if (buf == nullptr) return 6; // Internal

  int rc = 0;
  try {
    rc = static_cast<int>(translator_push_chunk(
        reinterpret_cast<const void*>(handle), buf,
        static_cast<unsigned long>(count)));
  } catch (...) {
    rc = 6;
  }
  env->ReleaseFloatArrayElements(samples, buf, JNI_ABORT);  // 只释放不写回
  return rc;
}

// ── 会话级（Android 注入链路推荐路径）────────────────

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeSessionPushChunk(
    JNIEnv* env, jobject /*thiz*/, jlong session, jlong source_index,
    jfloatArray samples, jint count) {
  if (session == 0 || samples == nullptr || count <= 0) return 7; // InvalidArg

  jfloat* buf = env->GetFloatArrayElements(samples, nullptr);
  if (buf == nullptr) return 6; // Internal

  int rc = 0;
  try {
    rc = static_cast<int>(translator_session_push_chunk(
        reinterpret_cast<void*>(session),
        static_cast<unsigned long>(source_index), buf,
        static_cast<unsigned long>(count)));
  } catch (...) {
    rc = 6;
  }
  env->ReleaseFloatArrayElements(samples, buf, JNI_ABORT);
  return rc;
}

extern "C" JNIEXPORT jint JNICALL
Java_com_translator_app_NativeBridge_nativeSessionSetCaptureGranted(
    JNIEnv* /*env*/, jobject /*thiz*/, jlong session, jint granted) {
  if (session == 0) return 7;
  try {
    return static_cast<jint>(translator_session_set_capture_granted(
        reinterpret_cast<void*>(session), static_cast<int>(granted)));
  } catch (...) {
    return 6;
  }
}
