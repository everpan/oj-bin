//! 结构层静态检查（§5.1 S002–S007；S001 由 `manifest::load_modules` 承担，
//! S007 由 `migrate::load_migrations` 承担）。`oj build` 内嵌全部 S*（fail build），
//! `oj build --check` 只校验不落盘（CI 门禁）。
//!
//! 报错三要素（§5.1 硬性要求）：违规文件路径（+规则 ID）、原因（引用具体声明）、
//! 下一步动作（S003 附 §3-D2 场景决策表）。SQL 表名提取与运行时守卫同一实现
//! （`bridge::guard::extract_tables`，§5.3 轻量扫描口径；`/* oj:allow-table=x */` 赦免）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::manifest::{self, Manifest};

/// 检查 `names` 列出的模块（归属图按 src 全部模块构建，S002 只报涉及被查模块的冲突；
/// 旁目录缺 manifest.yaml 不在此报——S001 归 load_modules/既有路径）。
/// view = 跨模块版本视图（dist/manifests.yaml 锁 ∪ src 各模块 manifest，build 已构造）。
/// 返回全部违规（一次给全，不逐条 fail-fast——CI 修复体验）。
pub fn run(src: &Path, names: &[String], view: &BTreeMap<String, String>) -> Result<(), String> {
    let mut v: Vec<String> = Vec::new();

    // 归属图：表 → 拥有模块（各模块 schema.yaml）+ manifest/schema 快照。
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    let mut schemas: BTreeMap<String, crate::schema::SchemaFile> = BTreeMap::new();
    let mut manifests: BTreeMap<String, Manifest> = BTreeMap::new();
    for (name, mdir) in scan_modules(src)? {
        let mf = mdir.join("manifest.yaml");
        if mf.is_file() {
            manifests.insert(name.clone(), manifest::parse_one(&mf)?);
        }
        if let Some(f) = crate::schema::SchemaFile::load(&mdir)? {
            for t in f.tables.keys() {
                if let Some(prev) = owner.insert(t.clone(), name.clone())
                    && (names.contains(&prev) || names.contains(&name))
                {
                    v.push(s002(t, &prev, &name));
                }
            }
            schemas.insert(name, f);
        }
    }

    for name in names {
        let mdir = src.join(name);
        let Some(m) = manifests.get(name) else {
            continue;
        };
        let deps = &m.deps;

        // S005：manifest.tables 与 schema.yaml 双向一致。
        let declared: BTreeSet<&str> = m.tables.iter().map(String::as_str).collect();
        let empty: BTreeSet<&str> = BTreeSet::new();
        let schema_tables = schemas.get(name).map(|f| {
            let s: BTreeSet<&str> = f.tables.keys().map(String::as_str).collect();
            s
        });
        let schema_tables = schema_tables.unwrap_or(empty);
        for t in declared.difference(&schema_tables) {
            v.push(format!(
                "S005: {name}/manifest.yaml: tables 声明 {t:?} 不在 schema.yaml\n  \
                 下一步：在 {name}/schema.yaml 补表 {t:?}，或从 manifest.tables 删除"
            ));
        }
        for t in schema_tables.difference(&declared) {
            v.push(format!(
                "S005: {name}/schema.yaml: 表 {t:?} 未列入 manifest.yaml 的 tables\n  \
                 下一步：manifest.yaml 补 tables: [{t}]（归属图以两处一致为准）"
            ));
        }

        // S003：模块 SQL（.ts 源码字符串 + seed.sql）引用他模块表且未声明 deps。
        let mut files: Vec<PathBuf> = collect_ts(&mdir);
        let seed = mdir.join("seed.sql");
        let has_seed = seed.is_file();
        if has_seed {
            files.push(seed.clone());
        }
        for f in &files {
            let text =
                std::fs::read_to_string(f).map_err(|e| format!("read {}: {e}", f.display()))?;
            let is_sql = f.extension().is_some_and(|e| e == "sql");
            for table in sql_tables(&text, is_sql) {
                let Some(o) = owner.get(&table) else {
                    continue; // 无主表（框架表/账本）不设防，同运行时 judge 口径
                };
                if o == name || deps.contains_key(o) {
                    continue;
                }
                v.push(s003(f, name, &table, o, deps));
            }
        }

        // S004：deps 版本范围被 view 满足。
        for (dep, range) in &m.deps {
            match view.get(dep).map(|v| semver::Version::parse(v)) {
                None => v.push(format!(
                    "S004: {name}/manifest.yaml: deps.{dep}:{range:?} 无满足版本\
                     （{dep} 不在 dist/manifests.yaml）\n  下一步：先 oj build {dep}"
                )),
                Some(Err(bad)) => v.push(format!(
                    "S004: {name}/manifest.yaml: deps.{dep} 目标版本不可解析（{bad}）\n  \
                     下一步：{dep}/manifest.yaml 的 version 须为 semver（如 0.1.0）"
                )),
                Some(Ok(vv)) => {
                    let want = semver::VersionReq::parse(range).map_err(|e| {
                        format!(
                            "S004: {name}/manifest.yaml: deps.{dep} 版本范围 {range:?} 非法：{e}\n  \
                             下一步：用 semver 范围（如 ^0.1.0）"
                        )
                    })?;
                    if !want.matches(&vv) {
                        v.push(format!(
                            "S004: {name}/manifest.yaml: deps.{dep}:{range} 无满足版本\
                             （dist 锁 {dep}={vv}）\n  下一步：调整 range 或升级 {dep}"
                        ));
                    }
                }
            }
        }

        // S006：seed.sql 禁 DDL / 禁非幂等 INSERT / 只碰本模块表与 deps 模块表。
        if has_seed {
            let text = std::fs::read_to_string(&seed)
                .map_err(|e| format!("read {}: {e}", seed.display()))?;
            for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let head = lead_word(stmt);
                if matches!(
                    head.as_str(),
                    "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "COMMENT"
                ) {
                    v.push(format!(
                        "S006: {}：seed.sql 含 DDL（{head} …）\n  \
                         下一步：结构演进写 migrations/NNNN__desc.sql 或 schema.yaml 声明",
                        seed.display()
                    ));
                }
                if head == "INSERT" {
                    let u = stmt.to_ascii_uppercase();
                    let idempotent = u.contains("OR IGNORE")
                        || u.contains("OR REPLACE")
                        || u.contains("ON CONFLICT")
                        || u.contains("ON DUPLICATE KEY");
                    if !idempotent {
                        v.push(format!(
                            "S006: {}：seed.sql 含非幂等 INSERT（seed 随启动重放）\n  \
                             下一步：改 INSERT OR IGNORE / ON CONFLICT DO UPDATE / OR REPLACE",
                            seed.display()
                        ));
                    }
                }
            }
            for table in sql_tables(&text, true) {
                if let Some(o) = owner.get(&table)
                    && o != name
                    && !deps.contains_key(o)
                {
                    v.push(format!(
                        "S006: {}：seed.sql 写入他模块表 {table:?}（属于 {o}）\n  \
                         下一步：种子数据移到 {o} 模块（归属模块自持种子）",
                        seed.display()
                    ));
                }
            }
        }
    }

    if v.is_empty() {
        Ok(())
    } else {
        Err(v.join("\n"))
    }
}

/// src 首层子目录扫描（宽容版 discover：缺 manifest.yaml 的目录跳过而非报错——
/// S001 属 load_modules/既有构建路径；YAML 解析错仍传播）。
fn scan_modules(src: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let rd = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.push((e.file_name().to_string_lossy().into_owned(), p));
        }
    }
    out.sort();
    Ok(out)
}

fn s002(table: &str, a: &str, b: &str) -> String {
    format!(
        "S002: 表 {table:?} 被多个模块声明（{a} 与 {b}）——归属图必须单射\n  \
         下一步：保留一处 schema.yaml 声明，另一方改为 deps 引用或上收 _platform"
    )
}

/// S003 文案附 §3-D2 场景决策表（spec 硬性要求）。
fn s003(
    f: &Path,
    module: &str,
    table: &str,
    owner: &str,
    deps: &std::collections::HashMap<String, String>,
) -> String {
    let hint = if deps.is_empty() {
        String::new()
    } else {
        let mut ks: Vec<&str> = deps.keys().map(String::as_str).collect();
        ks.sort_unstable();
        format!("（现有 deps 只含：{}）", ks.join(", "))
    };
    format!(
        "S003: {}: SQL 引用他模块表 {table:?}（属于 {owner}），模块 {module:?} 未声明依赖{hint}\n  \
         机制选择：取单条强一致=契约调用；列表/JOIN=读模型（bus 订阅+冗余表）；\
         同库只读=deps 声明+只读视图；框架共有表=上收 _platform\n  \
         下一步：manifest.yaml 补 deps: {{ {owner}: \"^<版本>\" }}，或赦免注释 /* oj:allow-table={table} */",
        f.display()
    )
}

/// 模块内全部 .ts 相对路径（S003 扫描面；fixtures/ 不在产物面不查）。
fn collect_ts(mdir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![mdir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if e.file_name() != "fixtures" {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|e| e == "ts") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 语句首词（跳过 `--` 行注释与空白；大小写归一）。
fn lead_word(stmt: &str) -> String {
    stmt.lines()
        .find(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with("--") && !t.starts_with("/*")
        })
        .and_then(|l| {
            l.trim_start()
                .split(|c: char| !c.is_ascii_alphabetic())
                .next()
                .map(|w| w.to_ascii_uppercase())
        })
        .unwrap_or_default()
}

/// SQL 表名提取（口径统一 guard::extract_tables）。.ts 源码先抠 JS 字符串/
/// 模板串内容（词法器对纯 SQL 跳字符串字面量，对源码恰要取串内 SQL——两口径）。
/// 含 `oj:allow-table=` 的串按名单赦免（§5.3 误报逃生门）。
fn sql_tables(text: &str, is_sql: bool) -> Vec<String> {
    if is_sql {
        return filter_allowed(&only_js::bridge::guard::extract_tables(text), text);
    }
    let mut out = Vec::new();
    for s in js_strings(text) {
        let tables = only_js::bridge::guard::extract_tables(&s);
        out.extend(filter_allowed(&tables, &s));
    }
    out
}

/// `oj:allow-table=a,b` 赦免名单（注释写在 SQL 内，即文本本身）。
fn filter_allowed(tables: &[String], text: &str) -> Vec<String> {
    let Some(i) = text.find("oj:allow-table=") else {
        return tables.to_vec();
    };
    let list: &str = text[i + "oj:allow-table=".len()..]
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ','))
        .next()
        .unwrap_or("");
    let allow: BTreeSet<&str> = list.split(',').map(str::trim).collect();
    tables
        .iter()
        .filter(|t| !allow.contains(t.as_str()))
        .cloned()
        .collect()
}

/// 抠 JS 字符串/模板串内容（'…' "…" `…`，`\` 转义；模板串 ${} 内嵌表达式原样保留，
/// 表名提取 best-effort 多抓不漏抓）。
fn js_strings(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                let start = i + 1;
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == q {
                        break;
                    } else {
                        i += 1;
                    }
                }
                out.push(src[start..i.min(b.len())].to_string());
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn manifest(name: &str, extra: &str) -> String {
        format!("name: {name}\ndesc: d\nversion: 0.1.0\n{extra}")
    }

    /// 夹具：src 下 user（account 表）与 order（orders 表 + 可注入文件）。
    fn fixture(tag: &str) -> PathBuf {
        let t = std::env::temp_dir().join(format!("oj-chk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(
            &t.join("src/user/manifest.yaml"),
            &manifest("user", "tables: [account]\n"),
        );
        write(
            &t.join("src/user/schema.yaml"),
            "tables:\n  account:\n    pk: id\n    columns:\n      id: { type: integer }\n",
        );
        write(
            &t.join("src/order/manifest.yaml"),
            &manifest("order", "tables: [orders]\n"),
        );
        write(
            &t.join("src/order/schema.yaml"),
            "tables:\n  orders:\n    pk: id\n    columns:\n      id: { type: integer }\n",
        );
        t
    }

    fn names(src: &Path) -> Vec<String> {
        manifest::load_modules(src)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect()
    }

    #[test]
    fn clean_project_passes() {
        let t = fixture("ok");
        write(
            &t.join("src/order/list/api.ts"),
            "function get(){ return db.query(\"select * from orders\"); }\nexport default { get };\n",
        );
        let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
        assert!(run(&t.join("src"), &names(&t.join("src")), &view).is_ok());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn s003_flags_cross_module_sql_and_allow_list() {
        let t = fixture("s003");
        write(
            &t.join("src/order/list/api.ts"),
            "import x from \"../user/api\";\nfunction get(){ return db.query('select * from account join orders using (id)'); }\nexport default { get };\n",
        );
        let view = BTreeMap::new();
        let e = run(&t.join("src"), &names(&t.join("src")), &view).unwrap_err();
        assert!(
            e.contains("S003") && e.contains("account") && e.contains("user"),
            "{e}"
        );
        assert!(e.contains("读模型"), "{e}"); // 决策表建议（§3-D2）
        // import from 的路径串不误报（js_strings 抠串内 SQL，路径无 SQL 关键词）。
        // 赦免注释生效。
        write(
            &t.join("src/order/list/api.ts"),
            "function get(){ return db.query('select * from account /* oj:allow-table=account */'); }\nexport default { get };\n",
        );
        assert!(run(&t.join("src"), &names(&t.join("src")), &view).is_ok());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn s003_seed_sql_and_deps_clear_it() {
        let t = fixture("s003d");
        write(
            &t.join("src/order/seed.sql"),
            "INSERT OR IGNORE INTO account VALUES (1);\n",
        );
        let e = run(&t.join("src"), &["order".to_string()], &BTreeMap::new()).unwrap_err();
        assert!(e.contains("S006") && e.contains("account"), "{e}");
        // deps 声明后放行（S003/S006 共用判定）。
        write(
            &t.join("src/order/manifest.yaml"),
            &manifest("order", "tables: [orders]\ndeps:\n  user: \"^0.1.0\"\n"),
        );
        let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
        assert!(run(&t.join("src"), &["order".to_string()], &view).is_ok());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn s004_version_range_checks() {
        let t = fixture("s004");
        write(
            &t.join("src/order/manifest.yaml"),
            &manifest("order", "tables: [orders]\ndeps:\n  user: \"^0.2.0\"\n"),
        );
        let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
        let e = run(&t.join("src"), &["order".to_string()], &view).unwrap_err();
        assert!(e.contains("S004") && e.contains("^0.2.0"), "{e}");
        // 范围满足 → 过。
        let view = BTreeMap::from([("user".into(), "0.2.1".into())]);
        assert!(run(&t.join("src"), &["order".to_string()], &view).is_ok());
        // 依赖缺失 / 目标版本非 semver / range 非法。
        let e = run(&t.join("src"), &["order".to_string()], &BTreeMap::new()).unwrap_err();
        assert!(e.contains("不在 dist/manifests.yaml"), "{e}");
        let view = BTreeMap::from([("user".into(), "legacy".into())]);
        let e = run(&t.join("src"), &["order".to_string()], &view).unwrap_err();
        assert!(e.contains("不可解析"), "{e}");
        write(
            &t.join("src/order/manifest.yaml"),
            &manifest("order", "tables: [orders]\ndeps:\n  user: \"latest\"\n"),
        );
        let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
        let e = run(&t.join("src"), &["order".to_string()], &view).unwrap_err();
        assert!(e.contains("非法"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn s005_tables_must_match_schema() {
        let t = fixture("s005");
        write(
            &t.join("src/order/manifest.yaml"),
            &manifest("order", "tables: [orders, ghost]\n"),
        );
        let e = run(&t.join("src"), &["order".to_string()], &BTreeMap::new()).unwrap_err();
        assert!(e.contains("S005") && e.contains("ghost"), "{e}");
        write(
            &t.join("src/order/manifest.yaml"),
            &manifest("order", "tables: []\n"),
        );
        let e = run(&t.join("src"), &["order".to_string()], &BTreeMap::new()).unwrap_err();
        assert!(e.contains("S005") && e.contains("orders"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn s006_rejects_ddl_nonidempotent_insert_and_foreign_tables() {
        let t = fixture("s006");
        write(
            &t.join("src/order/seed.sql"),
            "CREATE TABLE x (a);\nINSERT INTO orders VALUES (1);\nINSERT OR IGNORE INTO orders VALUES (2);\nINSERT INTO account VALUES (1);\n-- DROP TABLE y\n",
        );
        let e = run(&t.join("src"), &["order".to_string()], &BTreeMap::new()).unwrap_err();
        assert!(e.contains("S006") && e.contains("CREATE"), "{e}");
        assert!(e.contains("非幂等"), "{e}");
        assert!(e.contains("account") && e.contains("user"), "{e}");
        assert!(!e.contains("DROP"), "注释里的 DDL 不报：{e}");
        assert!(e.matches("S006").count() >= 3, "{e}"); // CREATE + 非幂等 INSERT + account
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn s002_reports_only_conflicts_touching_checked_modules() {
        let t = fixture("s002");
        // 第三方模块 other 与 user 冲突；只查 order → 不报；全查 → 报。
        write(
            &t.join("src/other/manifest.yaml"),
            &manifest("other", "tables: [account]\n"),
        );
        write(
            &t.join("src/other/schema.yaml"),
            "tables:\n  account:\n    pk: id\n    columns:\n      id: { type: integer }\n",
        );
        assert!(run(&t.join("src"), &["order".to_string()], &BTreeMap::new()).is_ok());
        let e = run(&t.join("src"), &names(&t.join("src")), &BTreeMap::new()).unwrap_err();
        assert!(
            e.contains("S002") && e.contains("user") && e.contains("other"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn js_strings_and_lead_word_basics() {
        let src = "const a = 'select 1'; const b = `from t2`; // 'skip'\nconst c = \"x\\\"y\";";
        assert_eq!(js_strings(src), ["select 1", "from t2", "x\\\"y"]);
        assert_eq!(lead_word("  -- c\nINSERT OR IGNORE INTO t"), "INSERT");
        assert_eq!(lead_word("create table x"), "CREATE");
        assert_eq!(lead_word(""), "");
    }
}
