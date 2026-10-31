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
