// ── 诊断用：把最近采集到的音频导出为 WAV ──

/// 采集侧保留的音频环形缓冲（秒）。
const DUMP_SECONDS: usize = 12;

/// 采集侧维护的诊断音频缓冲。
///
/// 保留最近 [`DUMP_SECONDS`] 秒的**归一化后**音频（16 kHz 单声道），
/// 供 [`translator_session_dump_capture_wav`] 导出。
/// 有了它，任何「识别不出来」的问题都能在 Python 侧复现
/// （波形、频谱、逐帧 VAD 判定），不必再靠猜。
pub struct CaptureDump {
    buf: Mutex<Vec<f32>>,
    rate: u32,
}

impl CaptureDump {
    pub fn new(rate: u32) -> Self {
        Self {
            buf: Mutex::new(Vec::new()),
            rate,
        }
    }

    /// 追加一块音频，超出窗口就丢最旧的。
    pub fn push(&self, samples: &[f32]) {
        let cap = DUMP_SECONDS * self.rate as usize;
        let mut b = match self.buf.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        b.extend_from_slice(samples);
        if b.len() > cap {
            let drop = b.len() - cap;
            b.drain(0..drop);
        }
    }

    /// 取当前缓冲的快照。
    pub fn snapshot(&self) -> (u32, Vec<f32>) {
        let b = self.buf.lock().map(|g| g.clone()).unwrap_or_default();
        (self.rate, b)
    }
}

/// 诊断用：把会话采集到的音频导出为 16-bit PCM WAV。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`；
/// `path` 必须是可写的 UTF-8 C 字符串路径。
#[no_mangle]
pub unsafe extern "C" fn translator_session_dump_capture_wav(
    handle: *mut c_void,
    path: *const c_char,
) -> i32 {
    ffi_guard!({
        if handle.is_null() || path.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let cap = match session.capture.lock() {
            Ok(c) => c,
            Err(_) => return FfiError::Internal as i32,
        };
        let Some(first) = cap.first() else {
            return FfiError::InvalidArg as i32;
        };
        let Some(dump) = first.capture_dump() else {
            return FfiError::Internal as i32;
        };
        let (rate, samples) = dump.snapshot();
        if samples.is_empty() {
            return FfiError::InvalidArg as i32;
        }
        let p = match std::ffi::CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => return FfiError::InvalidArg as i32,
        };
        if crate::audio::wav::write_wav_16k_mono(p, rate, &samples).is_err() {
            return FfiError::Internal as i32;
        }
        FfiError::Ok as i32
    })
}

