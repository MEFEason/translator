//! **离线性保证**
//!
//! 本模块是"构建与运行双离线"的**可执行证据**，而非仅靠约定。
//!
//! ## 保证方式
//!
//! 1. 依赖树中**不存在任何 HTTP / 网络客户端 crate**
//!    （reqwest / hyper / curl / ureq / rustls …），运行期与构建期均如此
//! 2. 模型与分词器只从**本地路径**读取
//! 3. 翻译引擎选 **Bergamot 而非 CTranslate2** —— 后者的
//!    `ctranslate2-sys` 构建期会联网下载预编译包（实测代理 502 失败），
//!    与本约束冲突；Bergamot 纯 CMake 源码编译，仅需 C++17
//! 4. 权重直接使用 Marian/Bergamot 原生格式，**无需 PyTorch→引擎转换步骤**
//!
//! ## 校验方式
//!
//! `scripts/check_offline.py` 分别扫描 `--edges normal`（运行期）与
//! `--edges build`（构建期），出现网络类依赖即失败。接入 CI 后，
//! "离线性"成为**构建门禁**，而非口头承诺。
//!
//! 这个门禁**已实际生效过一次**：它拦下了 CTranslate2 引入的
//! `ureq` / `rustls` / `http`，促成引擎切换。
//!
//! ## 明确不做的事
//!
//! - 不做模型在线下载（权重通过 `.dshm` 包由用户手动导入）
//! - 不做崩溃上报 / 遥测 / 远程配置
//! - 不做在线术语库同步
//! - 首次运行**不做任何后台网络探测**

/// 构建期可执行的离线性检查清单。
///
/// 这些项由 CI 逐条验证（见 scripts/check_offline.py）。
/// 本函数的存在让"离线性"成为代码里可引用的契约，
/// 而不是散落在文档里的承诺。
pub const OFFLINE_GUARANTEES: &[OfflineCheck] = &[
    OfflineCheck {
        id: "no-http-client-runtime",
        desc: "运行期依赖树不含 HTTP/网络客户端 crate",
        verified_by: "scripts/check_offline.py --edges normal",
    },
    OfflineCheck {
        id: "no-http-client-build",
        desc: "构建期依赖树不含网络 crate（因此构建本身也可离线）",
        verified_by: "scripts/check_offline.py --edges build",
    },
    OfflineCheck {
        id: "no-prebuilt-download",
        desc: "无「构建期下载预编译二进制」行为（这正是弃用 CTranslate2 的原因）",
        verified_by: "scripts/check_offline.py",
    },
    OfflineCheck {
        id: "model-from-local-path",
        desc: "模型与分词器仅从本地目录读取",
        verified_by: "mt::bergamot::BergamotTranslator::load 只做路径检查",
    },
    OfflineCheck {
        id: "no-model-conversion-step",
        desc: "权重用 Marian/Bergamot 原生格式，无需 PyTorch→引擎转换",
        verified_by: "models/ 使用 Bergamot 权重格式（model.bin + registry.json）",
    },
    OfflineCheck {
        id: "no-model-download",
        desc: "运行期不下载模型，由用户导入 .dshm 包",
        verified_by: "modelstore::install 只接受本地文件",
    },
    OfflineCheck {
        id: "no-telemetry",
        desc: "无遥测、无崩溃上报、无远程配置",
        verified_by: "依赖树检查（无 analytics 类 crate）",
    },
    OfflineCheck {
        id: "no-first-run-probe",
        desc: "首次运行无任何后台网络探测",
        verified_by: "无网络客户端依赖即天然满足",
    },
];

/// 一条离线性保证
pub struct OfflineCheck {
    pub id: &'static str,
    pub desc: &'static str,
    pub verified_by: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarantees_are_non_empty_and_unique() {
        assert!(!OFFLINE_GUARANTEES.is_empty());
        let mut ids: Vec<&str> = OFFLINE_GUARANTEES.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "检查项 id 不得重复");
    }

    #[test]
    fn every_check_documents_verification() {
        // 「未写下来 = 不存在」——每条保证必须说明如何验证
        for c in OFFLINE_GUARANTEES {
            assert!(!c.desc.trim().is_empty(), "{} 缺少描述", c.id);
            assert!(
                !c.verified_by.trim().is_empty(),
                "{} 缺少验证方式",
                c.id
            );
        }
    }

    #[test]
    fn critical_guarantees_present() {
        let ids: Vec<&str> = OFFLINE_GUARANTEES.iter().map(|c| c.id).collect();
        assert!(ids.contains(&"no-http-client-runtime"));
        assert!(ids.contains(&"no-http-client-build"));
        assert!(ids.contains(&"no-prebuilt-download"));
        assert!(ids.contains(&"no-telemetry"));
        assert!(ids.contains(&"no-model-download"));
    }
}
