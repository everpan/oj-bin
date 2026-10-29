//! G3 执行计划归一（`db.explain`，v0.1.108）。
//!
//! 只放三样：方言 EXPLAIN 前缀常量、PG 文本计划 / MySQL FORMAT=JSON 的归一纯函数、
//! `op_db_explain` 壳。壳 = 内层 SQL 过一遍租户守卫 → 拼前缀 → 直接调
//! [`db_query::query_impl`] 原函数（守卫/配额/trace/session 零复制），首行首列取
//! 结果 → 归一 PlanNode（spec：docs/superpowers/specs/2026-10-29-dbx-upstream-g3-u14-design.md）。
//!
//! 铁律（评审裁决 + 真机实测）：
//! 1. **无 ANALYZE 由构造保证**：前缀是宿主常量，用户只供内层语句——EXPLAIN 非
//!    ANALYZE 不执行内层语句，副作用为零。
//! 2. **PG 用文本计划，非 FORMAT JSON**：`EXPLAIN (FORMAT JSON)` 输出 json 类型列，
//!    sqlx Any 驱动不解（v0.1.108 真机实测，有无参数同样报错）；文本格式绑定参数
//!    可用。MySQL 维持 FORMAT=JSON（Any 可解，带参可用）。
//! 3. **参数值绝不内联**：绑定失败是方言能力问题，结构化报错，禁止拼进 SQL 文本。

/// PG 前缀：文本计划（默认格式）。Any 驱动限制见模块头铁律 2。
pub(crate) const PG_EXPLAIN_PREFIX: &str = "EXPLAIN ";
/// MySQL 前缀：JSON 格式，输出列可被 sqlx Any 解码。
pub(crate) const MYSQL_EXPLAIN_PREFIX: &str = "EXPLAIN FORMAT=JSON ";

#[cfg(test)]
mod tests {
    #[test]
    fn explain_prefixes_are_safe_by_construction() {
        assert!(!super::PG_EXPLAIN_PREFIX.contains("ANALYZE"));
        assert!(!super::MYSQL_EXPLAIN_PREFIX.contains("ANALYZE"));
        assert!(super::MYSQL_EXPLAIN_PREFIX.contains("FORMAT=JSON"));
        assert!(super::PG_EXPLAIN_PREFIX.ends_with(' '));
        assert!(super::MYSQL_EXPLAIN_PREFIX.ends_with(' '));
    }
}
