//! 统一错误类型。

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// 词库文件解析失败（含行号定位）
    GlossaryParse { file: String, line: u32, msg: String },
    /// 词库加载失败但已有可用快照
    GlossaryLoad(String),
    /// 模型缺失
    ModelMissing(String),
    /// 模型完整性校验失败（sha256 不匹配）
    ModelCorrupt(String),
    /// 磁盘空间不足
    DiskSpace { need: u64, have: u64 },
    /// 采集能力不可用
    CaptureUnavailable(String),
    /// 权限未授予
    PermissionDenied(String),
    /// 推理引擎初始化失败
    EngineInit(String),
    /// 推理执行失败
    Engine(String),
    /// 内部状态错误
    Internal(String),
    /// 配置非法（JSON 结构或字段值不合法）
    InvalidConfig(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::GlossaryParse { file, line, msg } => {
                write!(f, "词库解析失败 [{file}:{line}]: {msg}")
            }
            Error::GlossaryLoad(m) => write!(f, "词库加载失败: {m}"),
            Error::ModelMissing(m) => write!(f, "缺少模型: {m}"),
            Error::ModelCorrupt(m) => write!(f, "模型校验失败: {m}"),
            Error::DiskSpace { need, have } => write!(
                f,
                "磁盘空间不足: 需要 {} MB, 可用 {} MB",
                need / 1_048_576,
                have / 1_048_576
            ),
            Error::CaptureUnavailable(m) => write!(f, "音频采集不可用: {m}"),
            Error::PermissionDenied(m) => write!(f, "权限未授予: {m}"),
            Error::EngineInit(m) => write!(f, "推理引擎初始化失败: {m}"),
            Error::Engine(m) => write!(f, "推理执行失败: {m}"),
            Error::Internal(m) => write!(f, "内部错误: {m}"),
            Error::InvalidConfig(m) => write!(f, "配置非法: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
