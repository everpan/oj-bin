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
    manifest::load_modules(src, None, None)
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
    assert!(
        run(
            &t.join("src"),
            &names(&t.join("src")),
            &view,
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
    );
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
    let e = run(
        &t.join("src"),
        &names(&t.join("src")),
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
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
    assert!(
        run(
            &t.join("src"),
            &names(&t.join("src")),
            &view,
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
    );
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn s003_seed_sql_and_deps_clear_it() {
    let t = fixture("s003d");
    write(
        &t.join("src/order/seed.sql"),
        "INSERT OR IGNORE INTO account VALUES (1);\n",
    );
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S006") && e.contains("account"), "{e}");
    // deps 声明后放行（S003/S006 共用判定）。
    write(
        &t.join("src/order/manifest.yaml"),
        &manifest("order", "tables: [orders]\ndeps:\n  user: \"^0.1.0\"\n"),
    );
    let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
    assert!(
        run(
            &t.join("src"),
            &["order".to_string()],
            &view,
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
    );
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
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S004") && e.contains("^0.2.0"), "{e}");
    // 范围满足 → 过。
    let view = BTreeMap::from([("user".into(), "0.2.1".into())]);
    assert!(
        run(
            &t.join("src"),
            &["order".to_string()],
            &view,
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
    );
    // 依赖缺失 / 目标版本非 semver / range 非法。
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("不在 dist/manifests.yaml"), "{e}");
    let view = BTreeMap::from([("user".into(), "legacy".into())]);
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("不可解析"), "{e}");
    write(
        &t.join("src/order/manifest.yaml"),
        &manifest("order", "tables: [orders]\ndeps:\n  user: \"latest\"\n"),
    );
    let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
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
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S005") && e.contains("ghost"), "{e}");
    write(
        &t.join("src/order/manifest.yaml"),
        &manifest("order", "tables: []\n"),
    );
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
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
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S006") && e.contains("CREATE"), "{e}");
    assert!(e.contains("非幂等"), "{e}");
    assert!(e.contains("account") && e.contains("user"), "{e}");
    assert!(!e.contains("DROP"), "注释里的 DDL 不报：{e}");
    assert!(e.matches("S006").count() >= 3, "{e}"); // CREATE + 非幂等 INSERT + account
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn s006_scans_every_seed_variant_and_names_the_real_file() {
    let t = fixture("s006-dialect");
    write(
        &t.join("src/order/seed.sql"),
        "INSERT OR IGNORE INTO orders VALUES (1);\n",
    );
    // 方言件写 mysql 原生幂等写法 → 合法；pg 件里的 DDL → 必须报且点名该文件。
    write(
        &t.join("src/order/seed.mysql.sql"),
        "INSERT IGNORE INTO orders VALUES (2);\n",
    );
    write(&t.join("src/order/seed.pg.sql"), "CREATE TABLE x (a);\n");
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S006") && e.contains("CREATE"), "{e}");
    assert!(e.contains("seed.pg.sql"), "报错须点名真实文件：{e}");
    assert!(
        !e.contains("seed.mysql.sql"),
        "INSERT IGNORE 是幂等的，不应报：{e}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn insert_is_idempotent_accepts_native_dialect_forms() {
    // 通用件 / 未指明方言：保守口径，mysql 原生写法不认（它在 sqlite/pg 上是语法错）。
    for s in [
        "INSERT OR IGNORE INTO t VALUES (1)",
        "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
        "INSERT OR REPLACE INTO t VALUES (1)",
        // 注释/字符串里的关键字不算数：判据只看代码 token。
        "-- 建议改 INSERT IGNORE\nINSERT OR IGNORE INTO t VALUES (1)",
        "INSERT OR IGNORE INTO t VALUES ('INSERT IGNORE')",
        // 关键字被注释拆开也算（PG 允许注释出现在任意空白处）。
        "INSERT OR/*c*/IGNORE INTO t VALUES (1)",
        "INSERT INTO t VALUES (1) ON/*c*/CONFLICT DO NOTHING",
    ] {
        assert!(insert_is_idempotent(s), "{s}");
    }
    for s in [
        "INSERT INTO t VALUES (1)",
        // 前缀串不是关键字。
        "INSERT IGNORED INTO t VALUES (1)",
        "INSERT OR IGNORED INTO t VALUES (1)",
        "INSERT INTO t SELECT * FROM u",
        // mysql 原生写法在通用件上放行 = 起服才炸，必须拒绝。
        "INSERT IGNORE INTO t VALUES (1)",
        // 只在注释里出现 → 不能放行（重放会翻倍）。
        "INSERT INTO t VALUES (1) -- 可改 INSERT IGNORE",
        // 只在字符串字面量里出现 → 同理（单/双引号都是字面量，双引号在 mysql 默认模式是字符串）。
        "INSERT INTO t VALUES (\"OR IGNORE\")",
        "INSERT INTO t VALUES ('ON CONFLICT DO NOTHING')",
    ] {
        assert!(!insert_is_idempotent(s), "{s}");
    }
    // mysql 方言件：原生 INSERT IGNORE / ON DUPLICATE KEY 合法；它没有 ON CONFLICT。
    for s in [
        "INSERT IGNORE INTO t VALUES (1)",
        "INSERT IGNORE\nINTO t VALUES (1)",
        "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
        "INSERT OR IGNORE INTO t VALUES (1)",
    ] {
        assert!(
            insert_is_idempotent_in(s, Some(Dialect::MySql)),
            "mysql 方言件应认：{s}"
        );
    }
    for s in [
        "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT OR REPLACE INTO t VALUES (1)",
    ] {
        assert!(
            !insert_is_idempotent_in(s, Some(Dialect::MySql)),
            "mysql 方言件不该认：{s}"
        );
    }
    // pg 方言件：ON CONFLICT 合法；INSERT IGNORE / ON DUPLICATE KEY / OR REPLACE 不合法。
    assert!(insert_is_idempotent_in(
        "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
        Some(Dialect::Postgres)
    ));
    for s in [
        "INSERT IGNORE INTO t VALUES (1)",
        "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
        "INSERT OR REPLACE INTO t VALUES (1)",
    ] {
        assert!(
            !insert_is_idempotent_in(s, Some(Dialect::Postgres)),
            "pg 方言件不该认：{s}"
        );
    }
    // sqlite 方言件：OR REPLACE 合法；mysql 的两种写法都不合法。
    assert!(insert_is_idempotent_in(
        "INSERT OR REPLACE INTO t VALUES (1)",
        Some(Dialect::Sqlite)
    ));
    for s in [
        "INSERT IGNORE INTO t VALUES (1)",
        "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
    ] {
        assert!(
            !insert_is_idempotent_in(s, Some(Dialect::Sqlite)),
            "sqlite 方言件不该认：{s}"
        );
    }
}

#[test]
fn s006_rejects_misnamed_dialect_seed_file() {
    let t = fixture("s006-tag");
    write(
        &t.join("src/order/seed.postgres.sql"),
        "INSERT OR IGNORE INTO orders VALUES (1);\n",
    );
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S006") && e.contains("postgres"), "{e}");
    assert!(e.contains("seed.pg.sql"), "须给出正确标签：{e}");
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
    assert!(
        run(
            &t.join("src"),
            &["order".to_string()],
            &BTreeMap::new(),
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
    );
    let e = run(
        &t.join("src"),
        &names(&t.join("src")),
        &BTreeMap::new(),
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(
        e.contains("S002") && e.contains("user") && e.contains("other"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S008：别名目标必须存在；跨模块别名必须声明 deps（既有相对跨模块引用不追溯）。
#[test]
fn s008_alias_target_exists_and_cross_module_needs_deps() {
    let t = fixture("s008");
    let view = BTreeMap::from([("user".into(), "0.1.0".into())]);
    write(
        &t.join("src/order/_shared/util.ts"),
        "export const u = 1;\n",
    );
    // 本模块别名（含索引目录与显式后缀）→ 过
    write(
        &t.join("src/order/_shared/mod/index.ts"),
        "export const m = 1;\n",
    );
    write(
        &t.join("src/order/list/api.ts"),
        "import { u } from \"#_shared/util\";\nimport { m } from \"#_shared/mod\";\nimport { u2 } from \"#_shared/util.ts\";\nexport default {};\n",
    );
    for n in ["order", "user"] {
        assert!(
            run(
                &t.join("src"),
                &[n.to_string()],
                &view,
                only_js::bridge::SqlGuard::Off,
                None,
            )
            .is_ok(),
            "{n}"
        );
    }
    // 目标不存在 → S008（含探针候选清单）
    write(
        &t.join("src/order/list/api.ts"),
        "import { u } from \"#_shared/nope\";\nexport default {};\n",
    );
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S008") && e.contains("tried"), "{e}");
    // 跨模块别名未声明 deps → S008（给下一步）
    write(
        &t.join("src/user/_shared/validate.ts"),
        "export const v = 1;\n",
    );
    write(
        &t.join("src/order/list/api.ts"),
        "import { v } from \"#/user/_shared/validate\";\nexport default {};\n",
    );
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(
        e.contains("S008") && e.contains("deps") && e.contains("user") && e.contains("下一步"),
        "{e}"
    );
    // 声明 deps 后放行（S004 同时校验范围）
    write(
        &t.join("src/order/manifest.yaml"),
        &manifest("order", "tables: [orders]\ndeps:\n  user: \"^0.1.0\"\n"),
    );
    assert!(
        run(
            &t.join("src"),
            &["order".to_string()],
            &view,
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
    );
    // 未知模块名首段 → S008（列出实际存在的模块）
    write(
        &t.join("src/order/list/api.ts"),
        "import { v } from \"#/nope/x\";\nexport default {};\n",
    );
    let e = run(
        &t.join("src"),
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        None,
    )
    .unwrap_err();
    assert!(e.contains("S008") && e.contains("nope"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（PRD §7-4，评审乙）：索引下跨模块别名按 **模块名** 判归属——缺 `deps`
/// 仍报 S008，且点名模块名（`user`），目录名（`aa`）不参与身份、不进诊断。
#[test]
fn given_nested_module_roots_when_alias_cross_module_without_deps_then_s008_names_module() {
    let t = std::env::temp_dir().join(format!("oj-chk-idx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    // 目录名（aa/bb）与模块名（user/order）刻意不一致
    write(
        &t.join("src/aa/api/manifest.yaml"),
        &manifest("user", "tables: [account]\n"),
    );
    write(
        &t.join("src/aa/api/schema.yaml"),
        "tables:\n  account:\n    pk: id\n    columns:\n      id: { type: integer }\n",
    );
    write(
        &t.join("src/aa/api/_shared/validate.ts"),
        "export const v = 1;\n",
    );
    write(
        &t.join("src/bb/api/manifest.yaml"),
        &manifest("order", "tables: [orders]\n"),
    );
    write(
        &t.join("src/bb/api/schema.yaml"),
        "tables:\n  orders:\n    pk: id\n    columns:\n      id: { type: integer }\n",
    );
    write(
        &t.join("src/bb/api/list/api.ts"),
        "import { v } from \"#/user/_shared/validate\";\nexport default {};\n",
    );
    let src = t.join("src").canonicalize().unwrap();
    let idx = only_js::bridge::resolve_roots(&src, true, None).unwrap();
    let view = BTreeMap::from([
        ("user".to_string(), "0.1.0".to_string()),
        ("order".to_string(), "0.1.0".to_string()),
    ]);
    // When：跨模块别名未声明 deps
    let e = run(
        &src,
        &["order".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        Some(&idx),
    )
    .unwrap_err();
    // Then：S008 点名模块名与下一步；目录名不出现
    assert!(e.contains("S008") && e.contains("deps"), "{e}");
    assert!(e.contains("user"), "{e}");
    assert!(!e.contains("aa/"), "{e}");
    // 声明 deps 后放行（索引重建：manifest 已改）
    write(
        &t.join("src/bb/api/manifest.yaml"),
        &manifest("order", "tables: [orders]\ndeps:\n  user: \"^0.1.0\"\n"),
    );
    let idx = only_js::bridge::resolve_roots(&src, true, None).unwrap();
    assert!(
        run(
            &src,
            &["order".to_string()],
            &view,
            only_js::bridge::SqlGuard::Off,
            Some(&idx),
        )
        .is_ok()
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（评审甲）：索引下模块内再放 `manifest.yaml`（stray）→ S008 点名该模块，
/// 且**不**指向父模块根（否则等于鼓励把子目录当模块）。
#[test]
fn given_index_when_manifest_nested_inside_module_then_s008_points_at_the_module() {
    let t = std::env::temp_dir().join(format!("oj-chk-stray-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    write(
        &t.join("src/aa/api/manifest.yaml"),
        &manifest("user", "tables: [account]\n"),
    );
    write(
        &t.join("src/aa/api/schema.yaml"),
        "tables:\n  account:\n    pk: id\n    columns:\n      id: { type: integer }\n",
    );
    write(
        &t.join("src/aa/api/sub/manifest.yaml"),
        &manifest("sub", ""),
    );
    let src = t.join("src").canonicalize().unwrap();
    // 索引照建（stray 不阻断发现，交由 S008 报）
    let idx = only_js::bridge::resolve_roots(&src, true, None).unwrap();
    assert!(idx.get("sub").is_none(), "嵌套声明不得被认作模块");
    let view = BTreeMap::from([("user".to_string(), "0.1.0".to_string())]);
    let e = run(
        &src,
        &["user".to_string()],
        &view,
        only_js::bridge::SqlGuard::Off,
        Some(&idx),
    )
    .unwrap_err();
    assert!(e.contains("S008") && e.contains("sub"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// S008 边界（有意的不对称，防回归）：deps 门禁只约束**别名**新语法，
/// 既有**相对**跨模块引用不追溯——否则既有项目升级后 build 直接失败。
/// 相对写法的 release 版本绑定仍由构建期 view 保证。
#[test]
fn s008_does_not_gate_relative_cross_module_imports() {
    let t = fixture("s008rel");
    write(
        &t.join("src/user/_shared/validate.ts"),
        "export const v = 1;\n",
    );
    write(
        &t.join("src/order/list/api.ts"),
        "import { v } from \"../../user/_shared/validate\";\nexport default {};\n",
    );
    assert!(
        run(
            &t.join("src"),
            &["order".to_string()],
            &BTreeMap::new(),
            only_js::bridge::SqlGuard::Off,
            None,
        )
        .is_ok()
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

/// v0.1.125 根因修复（评审 S-blocker）：旧 lead_word 逐行找「首个不以 `--`/`/*`
/// 起头的行」，`/*x*/ DROP TABLE t` 整行被略过 → DDL / 非幂等 INSERT 双绕过。
/// 改用 code_tokens 后，块注释/行注释/字符串一概剥除，首词取真实关键字。
#[test]
fn lead_word_strips_inline_block_comment_bypass() {
    assert_eq!(lead_word("/*x*/ DROP TABLE t"), "DROP");
    assert_eq!(lead_word("/*c*/ INSERT INTO t VALUES (1)"), "INSERT");
    assert_eq!(lead_word("/* a\n b\n */ UPDATE t SET x = 1"), "UPDATE");
    assert_eq!(
        lead_word("-- heading\n/* block */\nINSERT OR IGNORE INTO t VALUES (1)"),
        "INSERT"
    );
    assert_eq!(lead_word("   "), "");
    assert_eq!(lead_word("-- only a comment\n"), "");
}

/// `--with-seed` 运行期 fail-closed 门禁：只放行幂等 INSERT，其余语句类型一律拒；
/// 按目标库方言判幂等（mysql 上 pg 的 ON CONFLICT 形态提前拒，不起服才 1064）。
#[test]
fn seed_stmt_allowed_is_fail_closed_and_dialect_aware() {
    // 放行：幂等 INSERT（含前置注释、方言件原生写法）。
    for s in [
        "INSERT OR IGNORE INTO t VALUES (1)",
        "/*c*/ INSERT OR IGNORE INTO t VALUES (1)",
        "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT OR REPLACE INTO t VALUES (1)",
    ] {
        assert!(seed_stmt_allowed(s, Dialect::Sqlite).is_ok(), "应放行：{s}");
    }
    // 拒：DDL（含 /*x*/ 绕过）、写/删/危险指令、事务、非幂等 INSERT、INSERT…SELECT。
    for s in [
        "/*x*/ DROP TABLE t",
        "CREATE TABLE x (a)",
        "ALTER TABLE t ADD COLUMN a int",
        "TRUNCATE TABLE t",
        "RENAME TABLE a TO b",
        "GRANT ALL ON t TO u",
        "UPDATE t SET a = 1",
        "DELETE FROM t",
        "ATTACH DATABASE 'x' AS y",
        "PRAGMA foreign_keys = OFF",
        "VACUUM",
        "BEGIN",
        "COMMIT",
        "CALL rebuild()",
        "INSERT INTO t VALUES (1)",
        "/*x*/ INSERT INTO t VALUES (1)",
        "INSERT OR IGNORE INTO t SELECT * FROM u",
    ] {
        assert!(
            seed_stmt_allowed(s, Dialect::Sqlite).is_err(),
            "应拒绝：{s}"
        );
    }
    // 按目标库方言：pg 的 ON CONFLICT 在 mysql 目标库上不是幂等写法，提前拒。
    assert!(
        seed_stmt_allowed(
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
            Dialect::MySql
        )
        .is_err()
    );
    // mysql 原生 INSERT IGNORE 在 mysql 目标库上放行。
    assert!(seed_stmt_allowed("INSERT IGNORE INTO t VALUES (1)", Dialect::MySql).is_ok());
    // 纯注释 chunk：无可执行的词，放行（不该把注释当危险语句）。
    assert!(seed_stmt_allowed("-- just a note", Dialect::Sqlite).is_ok());
}
