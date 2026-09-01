//! 模块级种子重放（spec P0）：`<module>/schema.sql` + `<module>/seed.sql`。
//! 语义与根 `seed.sql` 等同——幂等 SQL、仅 default 库且 sqlite、按 `;` 朴素切分
//! （语句内不得含分号字面量，§2.1）——仅拆到模块；执行顺序：根（deprecated）→
//! 各模块（目录名排序，schema 先于 seed）。
//! S002：同一张表被两处 `CREATE TABLE`（根 vs 模块、模块 vs 模块）→ 启动 fail-fast，
//! 不静默合并（§8-1）。fixtures/ 不重放（演示数据，P1 起由 `oj fixture` 灌入）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::{DataAccessor, Dialect};

/// 模块内种子文件（重放顺序 = 数组顺序：结构在前、数据在后）。
const SEED_FILES: [&str; 2] = ["schema.sql", "seed.sql"];

/// 收集 `dir` 首层各模块的种子文件（dev: `src/<m>/`，release: `dist/<m>-<v>/`），
/// 按模块名排序、模块内按 SEED_FILES 顺序。返回 (模块标签=目录名, 文件路径)。
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
        for f in SEED_FILES {
            let p = d.join(f);
            if p.is_file() {
                out.push((d.file_name().unwrap().to_string_lossy().into_owned(), p));
            }
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

/// 根 seed.sql 的 deprecation 文案（§8-1：并存期警告，指向迁移去处）。
fn deprecation_note(tables: &[String]) -> String {
    if tables.is_empty() {
        "warn: root seed.sql 已废弃：内容为空，请直接删除（模块种子见 src/<module>/seed.sql）"
            .to_string()
    } else {
        format!(
            "warn: root seed.sql 已废弃：请将 {} 迁至对应模块的 seed.sql（同名表并存将报 S002）",
            tables.join(", ")
        )
    }
}

/// 启动重放（from_config 调用）：根 `config_dir/seed.sql`（deprecated）→ 模块种子。
/// 无任何种子文件 → 静默返回；有种子但 default 库缺失/非 sqlite → warn 跳过
/// （与今日根 seed 行为一致）。先全量冲突检查（S002）再执行——失败不落任何副作用。
pub async fn replay_all(
    default: Option<&Arc<dyn DataAccessor>>,
    config_dir: &Path,
    dir: &Path,
) -> Result<(), String> {
    let root_path = config_dir.join("seed.sql");
    let has_root = root_path.is_file();
    let modules = collect(dir);
    if !has_root && modules.is_empty() {
        return Ok(());
    }
    let db = match default {
        Some(d) if d.dialect() == Dialect::Sqlite => d,
        _ => {
            eprintln!("warn: seed skipped (default db is not sqlite)");
            return Ok(());
        }
    };
    // 读入全部文本（根在前）。
    let mut files: Vec<PathBuf> = Vec::new();
    if has_root {
        files.push(root_path.clone());
    }
    files.extend(modules.iter().map(|(_, p)| p.clone()));
    let mut texts = Vec::new();
    for p in &files {
        let t = std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", show(p)))?;
        texts.push((p.clone(), t));
    }
    // S002 冲突检查：表 → 首见文件；重复即 fail-fast（不执行任何语句）。
    let mut owner: HashMap<&str, &Path> = HashMap::new();
    for (p, t) in &texts {
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
    if has_root {
        let root_tables = create_tables(&texts[0].1);
        if !split_statements(&texts[0].1).is_empty() {
            eprintln!("{}", deprecation_note(&root_tables));
        }
    }
    for (p, t) in &texts {
        for stmt in split_statements(t) {
            db.exec_with_params(stmt, &[])
                .await
                .map_err(|e| format!("seed {}: {e}", show(p)))?;
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
    fn deprecation_note_lists_tables() {
        assert!(deprecation_note(&["account".into()]).contains("account"));
        assert!(deprecation_note(&[]).contains("已废弃"));
    }

    /// 测试辅助：真 sqlite 内存库 + 项目夹具，跑 replay_all 后回读建表清单。
    async fn replay(root: &Path, dir: &Path) -> Result<Vec<serde_json::Value>, String> {
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        let db = reg
            .connect("sqlite::memory:", root)
            .await
            .map_err(|e| e.to_string())?;
        replay_all(Some(&db), root, dir).await?;
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
    async fn replays_root_then_modules_in_order() {
        let t = std::env::temp_dir().join(format!("oj-seed-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        // 根 + 两个模块（user 建表插入；order 的 seed 引用 user 的表 → 模块顺序生效）
        write(t.join("seed.sql"), "CREATE TABLE IF NOT EXISTS r (x);");
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
            names.contains(&"r") && names.contains(&"account") && names.contains(&"orders"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schema_sql_runs_before_seed_sql() {
        let t = std::env::temp_dir().join(format!("oj-seed-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        // seed.sql INSERT 依赖 schema.sql 建的表 → 顺序错即报错
        write(t.join("src/u/schema.sql"), "CREATE TABLE m (x);");
        write(t.join("src/u/seed.sql"), "INSERT INTO m VALUES (1);");
        let tables = replay(&t, &t.join("src")).await.unwrap();
        assert_eq!(tables.len(), 1, "{tables:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s002_root_vs_module_conflict_fails_before_exec() {
        let t = std::env::temp_dir().join(format!("oj-seed-s002a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(
            t.join("seed.sql"),
            "CREATE TABLE IF NOT EXISTS account (id);",
        );
        write(
            t.join("src/user/seed.sql"),
            "CREATE TABLE IF NOT EXISTS account (id);",
        );
        let e = replay(&t, &t.join("src")).await.unwrap_err();
        assert!(e.contains("S002") && e.contains("account"), "{e}");
        assert!(e.contains("seed.sql") && e.contains("下一步"), "{e}");
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

    #[tokio::test(flavor = "current_thread")]
    async fn non_sqlite_default_skips_with_no_error() {
        let t = std::env::temp_dir().join(format!("oj-seed-dial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(t.join("seed.sql"), "CREATE TABLE r (x);");
        // InMemoryAccessor：dialect() 默认 Sqlite —— 换用无 default 库的 None 分支验证跳过。
        replay_all(None, &t, &t.join("src")).await.unwrap();
        let _ = std::fs::remove_dir_all(&t);
    }
}
