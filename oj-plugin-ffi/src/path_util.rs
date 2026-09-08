//! 跨平台路径 → 连接串归一（宿主与插件共用：两侧都依赖本 crate）。
//!
//! Windows 上拼 sqlite DSN 有两个坑，收在本模块一次性处理：
//! 1. `canonicalize()` 返回 verbatim 前缀 `\\?\D:\...`，其 `\\?\` 经反斜杠转
//!    正斜杠变成 `//?/`，sqlx 会把整条路径解析成 query 参数。用 `dunce` 剥掉
//!    （unix 上是 no-op，直接原样返回）。
//! 2. 绝对路径必须用单冒号 `sqlite:` + 正斜杠承载：`sqlite://C:\...` 经 Any
//!    驱动内部的 Url 解析会把盘符吞成 host（`sqlite://C/...`），sqlite 随之按
//!    相对路径开库 → SQLITE_CANTOPEN（code 14）。

use std::path::Path;

/// 数据库文件路径 → sqlite 连接串。
pub fn sqlite_file_dsn(p: &Path) -> String {
    let p = dunce::simplified(p);
    format!("sqlite:{}", p.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单冒号 + 正斜杠是硬契约：`sqlite://` 会让 Url 解析吞掉盘符（host）。
    #[test]
    fn given_path_when_sqlite_file_dsn_then_single_colon_forward_slash() {
        assert_eq!(sqlite_file_dsn(Path::new("a\\b.db")), "sqlite:a/b.db");
        assert_eq!(sqlite_file_dsn(Path::new("/tmp/x.db")), "sqlite:/tmp/x.db");
    }
}
