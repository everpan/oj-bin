//! 模块级种子重放（spec P0）：`<module>/seed.sql`，三方言 default 库随启动重放。
//! 幂等 SQL、按 `;` 朴素切分（语句内不得含分号字面量，§2.1）；执行顺序 = 模块目录名
//! 排序。幂等写法以 sqlite 惯用法为源（`INSERT OR IGNORE`），引擎按目标方言自动改写
//! 关键字（mysql → `INSERT IGNORE`、pg → 句尾 `ON CONFLICT DO NOTHING`）。
//! 每条语句的执行与结果（受影响行数）记 tracing 日志（server 落 logs/，CLI 落 stderr）。
//! S002：同一张表被两处 `CREATE TABLE` → 启动 fail-fast，不静默合并（§8-1）。
//! fixtures/ 不重放（演示数据，由 `oj fixture` 灌入）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::{DataAccessor, Dialect};

/// 模块种子文件名。
const SEED_FILE: &str = "seed.sql";

/// 收集 `dir` 首层各模块的 `seed.sql`（dev: `src/<m>/`，release: `dist/<m>-<v>/`），
/// 按模块名排序。返回 (模块标签=目录名, 文件路径)。
pub fn collect(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for d in dirs {
        let p = d.join(SEED_FILE);
        if p.is_file() {
            out.push((d.file_name().unwrap().to_string_lossy().into_owned(), p));
        }
    }
    out
}

/// 提取 SQL 文本里 `CREATE TABLE` 的表名（去 `--` 行注释后扫描；
/// `CREATE INDEX` 与 `INSERT INTO` 不算建表）。认 `IF NOT EXISTS`、引号
/// （"t" / \`t\` / [t]）与 schema 限定（main.t → t）。重复出现不去重（调用方只比对首见）。
pub fn create_tables(sql: &str) -> Vec<String> {
    // 去注释：整行以 -- 开头的行剔除（语句中段的行内注释不常见，不做）。
    let code = sql
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find_ci(&code, "create table", i) {
        let rest = code[p + "create table".len()..].trim_start();
        let rest = if find_ci(rest, "if not exists", 0) == Some(0) {
            rest["if not exists".len()..].trim_start()
        } else {
            rest
        };
        if let Some(name) = take_ident(rest) {
            out.push(name);
        }
        i = p + "create table".len();
    }
    out
}

/// 大小写不敏感子串查找（ASCII 关键字；UTF-8 续字节 ≥ 0x80 不会误命中 ASCII）。
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// 从位置 0 取标识符：引号包裹（"t" / \`t\` / [t]）或裸词；带 schema 限定
/// （`main.t` / `"main".t`）取末段。非标识符开头 → None。
fn take_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let (raw, rest) = match s.chars().next()? {
        '"' => quoted(s, '"')?,
        '`' => quoted(s, '`')?,
        '[' => {
            let e = s.find(']')?;
            (&s[1..e], &s[e + 1..])
        }
        _ => {
            let e = s
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(s.len());
            if e == 0 {
                return None;
            }
            (&s[..e], &s[e..])
        }
    };
    let rest = rest.trim_start();
    if let Some(dot_rest) = rest.strip_prefix('.') {
        return Some(take_ident(dot_rest).unwrap_or_else(|| raw.to_string()));
    }
    Some(raw.to_string())
}

/// 取 `q` 包裹的片段，返回 (内容, 结束后的剩余串)。
fn quoted(s: &str, q: char) -> Option<(&str, &str)> {
    let e = s[1..].find(q)? + 1;
    Some((&s[1..e], &s[e + 1..]))
}

/// 诊断文案里的路径：统一以 `/` 分隔。Windows 原生分隔符 `\` 会让同一条报错在
/// 跨平台下不可比对（测试按 `模块/seed.sql` 断言）。
fn show(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `;` 朴素切分 + 去空（继承 §2.1 约束：语句内不得含分号字面量）。
fn split_statements(text: &str) -> Vec<&str> {
    text.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// seed 幂等写法以 sqlite 惯用法为源：`INSERT OR IGNORE INTO …`。按目标方言改写
/// 关键字：mysql → `INSERT IGNORE`；pg → 剥 `OR IGNORE`、句尾追加 `ON CONFLICT DO
/// NOTHING`。其余形态（`OR REPLACE` / `ON CONFLICT` / `ON DUPLICATE KEY`）与非
/// INSERT 语句一律原样透传——作者显式选择的方言写法不猜测改写。
pub fn translate_insert<'a>(stmt: &'a str, d: Dialect) -> Cow<'a, str> {
    if d == Dialect::Sqlite {
        return Cow::Borrowed(stmt);
    }
    // 仅当语句以 INSERT 起头、紧跟空白 + OR IGNORE + 词边界时改写。
    if !stmt
        .get(..6)
        .is_some_and(|h| h.eq_ignore_ascii_case("INSERT"))
        || !stmt[6..].starts_with(|c: char| c.is_ascii_whitespace())
    {
        return Cow::Borrowed(stmt);
    }
    let after = stmt[6..].trim_start();
    if !after
        .get(..9)
        .is_some_and(|h| h.eq_ignore_ascii_case("or ignore"))
        || !after[9..].starts_with(|c: char| c.is_ascii_whitespace())
    {
        return Cow::Borrowed(stmt);
    }
    let rest = after[9..].trim_start();
    match d {
        Dialect::MySql => Cow::Owned(format!("INSERT IGNORE {rest}")),
        _ => Cow::Owned(format!("INSERT {rest} ON CONFLICT DO NOTHING")),
    }
}

/// 启动重放（from_config 调用）：各模块 `seed.sql`（目录名排序）。
/// 无种子文件 → 静默返回；有种子但 default 库缺失 → warn 跳过。
/// 三方言都重放（幂等靠 S006 门禁 + 方言改写 + SQL 自身）；先全量冲突检查（S002）
/// 再执行——失败不落任何副作用。
pub async fn replay_all(default: Option<&Arc<dyn DataAccessor>>, dir: &Path) -> Result<(), String> {
    let modules = collect(dir);
    if modules.is_empty() {
        return Ok(());
    }
    let Some(db) = default else {
        tracing::warn!("seed skipped: no default db");
        eprintln!("warn: seed skipped (no default db)");
        return Ok(());
    };
    let mut texts = Vec::new();
    for (name, p) in &modules {
        let t = std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", show(p)))?;
        texts.push((name.as_str(), p.clone(), t));
    }
    // S002 冲突检查：表 → 首见文件；重复即 fail-fast（不执行任何语句）。
    let mut owner: HashMap<&str, &Path> = HashMap::new();
    for (_, p, t) in &texts {
        for name in create_tables(t) {
            if let Some(prev) = owner.get(name.as_str()) {
                return Err(format!(
                    "S002: 表 `{name}` 被多处建表：{} 与 {}\n  \
                     原因：模块自治要求 表→模块 单射（§8-1），不静默合并。\n  \
                     下一步：保留唯一归属处的建表与数据，删除另一处后重试。",
                    show(prev),
                    show(p)
                ));
            }
            owner.insert(leak_str(name), p.as_path());
        }
    }
    for (name, p, t) in &texts {
        for (i, stmt) in split_statements(t).into_iter().enumerate() {
            let sql = translate_insert(stmt, db.dialect());
            match db.exec_with_params(&sql, &[]).await {
                Ok(rows) => tracing::info!(
                    module = name,
                    file = %show(p),
                    seq = i,
                    rows,
                    stmt = %crate::migrate::log_snip(stmt),
                    "seed ok"
                ),
                Err(e) => {
                    tracing::error!(
                        module = name,
                        file = %show(p),
                        seq = i,
                        stmt = %crate::migrate::log_snip(stmt),
                        "seed failed: {e}"
                    );
                    return Err(format!("seed {}: {e}", show(p)));
                }
            }
        }
    }
    Ok(())
}

/// 借用表名进 owner 的键（进程级启动期一次性，泄漏量 = 建表数，可忽略）。
fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_tables_extracts_names() {
        // 基本形态：IF NOT EXISTS / 大小写混合 / 多语句
        assert_eq!(
            create_tables("CREATE TABLE IF NOT EXISTS a (id INTEGER);\ncreate table B( id text );"),
            vec!["a", "B"]
        );
        // 引号三形态 + schema 限定取末段
        assert_eq!(
            create_tables(
                "CREATE TABLE \"q1\" (x); CREATE TABLE `q2` (x); CREATE TABLE [q3] (x); \
                           CREATE TABLE main.qualified (x); CREATE TABLE \"main\".\"qd\" (x);"
            ),
            vec!["q1", "q2", "q3", "qualified", "qd"]
        );
        // INSERT / CREATE INDEX / 注释行不算
        assert_eq!(
            create_tables(
                "-- CREATE TABLE commented (x);\nINSERT INTO t (x) VALUES (1);\nCREATE INDEX idx ON t (x);"
            ),
            Vec::<String>::new()
        );
        // 空文本 / 垃圾输入不 panic
        assert_eq!(create_tables(""), Vec::<String>::new());
        assert_eq!(create_tables("create table"), Vec::<String>::new());
    }

    #[test]
    fn split_keeps_inheritance_of_semicolon_rule() {
        assert_eq!(split_statements("a; b;;"), vec!["a", "b"]);
        assert!(split_statements("  ;; ").is_empty());
    }

    #[test]
    fn translate_insert_rewrites_per_dialect() {
        let s = "INSERT OR IGNORE INTO t (a) VALUES (1)";
        assert_eq!(translate_insert(s, Dialect::Sqlite), s);
        assert_eq!(
            translate_insert(s, Dialect::MySql),
            "INSERT IGNORE INTO t (a) VALUES (1)"
        );
        assert_eq!(
            translate_insert(s, Dialect::Postgres),
            "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING"
        );
        // 大小写不敏感；保留作者原文的其余部分。
        assert_eq!(
            translate_insert("insert or ignore into t values (1)", Dialect::MySql),
            "INSERT IGNORE into t values (1)"
        );
        // 非 INSERT / 其他幂等形态 / 无词边界 → 原样透传。
        assert_eq!(
            translate_insert("UPDATE t SET a = 1", Dialect::Postgres),
            "UPDATE t SET a = 1"
        );
        assert_eq!(
            translate_insert("INSERT INTO t VALUES (1)", Dialect::MySql),
            "INSERT INTO t VALUES (1)"
        );
        assert_eq!(
            translate_insert("INSERT OR REPLACE INTO t VALUES (1)", Dialect::Postgres),
            "INSERT OR REPLACE INTO t VALUES (1)"
        );
        assert_eq!(
            translate_insert("INSERT OR IGNORED INTO t VALUES (1)", Dialect::MySql),
            "INSERT OR IGNORED INTO t VALUES (1)"
        );
    }

    /// 测试辅助：真 sqlite 内存库 + 项目夹具，跑 replay_all 后回读建表清单。
    async fn replay(root: &Path, dir: &Path) -> Result<Vec<serde_json::Value>, String> {
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        let db = reg
            .connect("sqlite::memory:", root)
            .await
            .map_err(|e| e.to_string())?;
        replay_all(Some(&db), dir).await?;
        db.query_with_params(
            "select name from sqlite_master where type='table' order by name",
            &[],
        )
        .await
        .map_err(|e| e.to_string())
    }

    fn write(p: PathBuf, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replays_modules_in_order() {
        let t = std::env::temp_dir().join(format!("oj-seed-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        // 两个模块（user 建表插入；order 的 seed 引用 user 的表 → 模块顺序生效）
        write(
            t.join("src/user/seed.sql"),
            "CREATE TABLE IF NOT EXISTS account (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\
             \nINSERT OR IGNORE INTO account (id, name) VALUES (1, 'neo');",
        );
        write(
            t.join("src/order/seed.sql"),
            "CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY, account_id INTEGER);\
             \nINSERT OR IGNORE INTO orders VALUES (1, 1);",
        );
        let tables = replay(&t, &t.join("src")).await.unwrap();
        let names: Vec<&str> = tables.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            names.contains(&"account") && names.contains(&"orders"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_default_db_skips_with_no_error() {
        let t = std::env::temp_dir().join(format!("oj-seed-dial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(t.join("src/u/seed.sql"), "CREATE TABLE r (x);");
        // 无 default 库 → warn 跳过（有库时三方言都重放，无方言豁免）。
        replay_all(None, &t).await.unwrap();
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s002_module_vs_module_conflict() {
        let t = std::env::temp_dir().join(format!("oj-seed-s002b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(
            t.join("src/a/seed.sql"),
            "CREATE TABLE IF NOT EXISTS shared (id);",
        );
        write(t.join("src/b/seed.sql"), "create table shared (id);");
        let e = replay(&t, &t.join("src")).await.unwrap_err();
        assert!(
            e.contains("S002") && e.contains("a/seed.sql") && e.contains("b/seed.sql"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_seeds_is_silent_ok() {
        let t = std::env::temp_dir().join(format!("oj-seed-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/u")).unwrap();
        let tables = replay(&t, &t.join("src")).await.unwrap();
        assert!(tables.is_empty());
        let _ = std::fs::remove_dir_all(&t);
    }
}
