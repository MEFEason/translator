//! 词库子系统（需求 5，核心）。
//!
//! 三层作用机制（详见 docs/GLOSSARY.md §4）：
//!
//! ```text
//!        英文 ASR 文本
//!             │
//!     ┌───────▼────────┐  Layer A
//!     │ ASR 热词偏置   │  词库喂给解码器 → 术语被听对
//!     └───────┬────────┘
//!     ┌───────▼────────┐  Layer B
//!     │ 哨兵保护       │  Aho-Corasick 最长匹配 → ▁X{n}X
//!     └───────┬────────┘
//!     ┌───────▼────────┐
//!     │ 离线翻译 en→zh │  Marian 原样透传哨兵
//!     └───────┬────────┘
//!     ┌───────▼────────┐  Layer C
//!     │ 哨兵还原+兜底  │
//!     └───────┬────────┘
//!             │
//!        最终中文译文
//! ```

pub mod parse;
pub mod schema;
pub mod sentinel;
pub mod snapshot;

pub use schema::{Entry, GlossaryFile, LoadOutcome, Scope};
pub use sentinel::{GlossaryMatcher, RestorePass, SentinelPass};
pub use snapshot::{
    GlossaryManager, GlossarySnapshot, HitStats, PendingSegment, SnapshotGuard,
};
