//! 任务式批量导入（REQ-12，v0.1.115）：`db.import/importStatus/importCancel`。
//!
//! T1 占位骨架：仅 `ImportRegistry` 空壳（Default），供装配接线编译；
//! T2 落登记/配额/状态/取消/清扫，T3 解析器，T4 渲染，T5 三 op，T6 consumer。

/// 导入任务登记表（T1 占位：T2 充实为 ExportRegistry 镜像，`imp_` 前缀隔离）。
#[derive(Debug, Default)]
pub struct ImportRegistry;

impl ImportRegistry {
    /// 占位构造（T2 换 `new(&DbImportSection)` 同 export 模式）。
    pub fn new(_section: &crate::bridge::db_policy::DbImportSection) -> Self {
        Self
    }
}
