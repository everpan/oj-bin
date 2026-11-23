use super::*;

use std::path::Path;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "oj-mcmd-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 与 `src/bridge/ffi.rs::triple()` 一致——`<plugins_dir>/<triple>/` 才是扫描目录。
/// 测试须把插件扫描隔离到空目录，否则 workspace 自带（或 CI 检出）的 bin/plugins 里
/// 若含 ABI 不符的陈旧产物，会在迁移/对账逻辑前抢先报错（仅 Windows 主机三元组命中）。
fn host_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        other => format!("{arch}-unknown-{other}-gnu"),
    }
}

/// 写 config.yaml：`profiles` = db 段 name→DSN；插件扫描隔离到空 `<triple>` 目录
/// （隔离理由见 `host_triple`）。
fn write_config(t: &Path, profiles: &[(&str, &str)]) {
    let plugins_base = t.join("plugins-isolated");
    std::fs::create_dir_all(plugins_base.join(host_triple())).unwrap();
    let plugins_dir = plugins_base.to_string_lossy().replace('\\', "/");
    let mut db = String::new();
    for (name, dsn) in profiles {
        db.push_str(&format!("  {name}: {dsn}\n"));
    }
    std::fs::write(
        t.join("config.yaml"),
        format!("db:\n{db}plugins_dir: \"{plugins_dir}\"\n"),
    )
    .unwrap();
}

/// 夹具：项目根（config.yaml + src/m/{manifest,migrations,fixtures}）。
/// config 声明两个 profile：`default`（db.sqlite）+ `test`（db_test.sqlite）——供 `--db` 用。
fn project(tag: &str) -> PathBuf {
    let t = tmpdir(tag);
    let d = format!("sqlite://{}/db.sqlite", t.display());
    let x = format!("sqlite://{}/db_test.sqlite", t.display());
    write_config(&t, &[("default", &d), ("test", &x)]);
    std::fs::create_dir_all(t.join("src/m/migrations")).unwrap();
    std::fs::create_dir_all(t.join("src/m/fixtures")).unwrap();
    std::fs::write(
        t.join("src/m/manifest.yaml"),
        "name: m\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m/migrations/0001__init.sql"),
        "CREATE TABLE g (x);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m/fixtures/demo.sql"),
        "INSERT OR IGNORE INTO g VALUES (1);",
    )
    .unwrap();
    t
}

/// 夹具：双模块双库——`core` 无绑定（→default），`ao` 绑定 `vectors`。
/// REQ-HNSW Phase 1：迁移按 manifest `db:` 段路由到命名连接。
fn project_bound(tag: &str) -> PathBuf {
    let t = tmpdir(tag);
    let d = format!("sqlite://{}/db.sqlite", t.display());
    let v = format!("sqlite://{}/db_vectors.sqlite", t.display());
    write_config(&t, &[("default", &d), ("vectors", &v)]);
    for (m, manifest) in [
        ("core", "name: core\ndesc: d\nversion: 0.1.0\n"),
        ("ao", "name: ao\ndesc: d\nversion: 0.1.0\ndb: vectors\n"),
    ] {
        std::fs::create_dir_all(t.join(format!("src/{m}/migrations"))).unwrap();
        std::fs::write(t.join(format!("src/{m}/manifest.yaml")), manifest).unwrap();
    }
    std::fs::write(
        t.join("src/core/migrations/0001__init.sql"),
        "CREATE TABLE g (x);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/ao/migrations/0001__init.sql"),
        "CREATE TABLE v (x);",
    )
    .unwrap();
    t
}

/// 便捷构造：标准 migrate 调用（其余旗标缺省）。
fn mig_args(t: &Path, db: Option<&str>) -> MigrateArgs {
    MigrateArgs {
        site: None,
        config: Some(cfg(t)),
        dir: src(t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: db.map(|s| s.to_string()),
        format: None,
    }
}

/// 项目 config 路径 + src 目录（测试里反复用）。
fn cfg(t: &Path) -> String {
    t.join("config.yaml").display().to_string()
}

fn src(t: &Path) -> Option<String> {
    Some(t.join("src").display().to_string())
}

/// 目标 sqlite 文件里是否存在某表（文件不存在则 sqlite 现建，查询返回空 → false）。
async fn has_table_in(db: &Path, name: &str) -> bool {
    let acc = only_js::bridge::DbBackendRegistry::builtin()
        .connect(&format!("sqlite://{}", db.display()), db.parent().unwrap())
        .await
        .unwrap();
    acc.query(&format!(
        "select name from sqlite_master where type='table' and name='{name}'"
    ))
    .await
    .unwrap()
    .len()
        == 1
}

async fn has_table(t: &Path, name: &str) -> bool {
    has_table_in(&t.join("db.sqlite"), name).await
}

/// CLI 全链路：run_migrate 建表 + 账本；重跑幂等；run_fixture 灌数。
#[tokio::test(flavor = "current_thread")]
async fn migrate_and_fixture_end_to_end() {
    let t = project("e2e");
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap();
    assert!(has_table(&t, "g").await);
    assert!(has_table(&t, "_oj_migrations").await);
    // 幂等：重跑不增不改。
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap();
    // fixture：演示数据进表（不进账本）。
    run_fixture(&FixtureArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        module: None,
        db: None,
    })
    .await
    .unwrap();
    // 未知模块 fail-fast。
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: Some("ghost".into()),
        db: None,
        format: None,
    })
    .await
    .unwrap_err();
    assert!(e.contains("ghost"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// --db（v0.1.21）：目标库 = config `db:` 段的 profile；未声明库名 fail-fast（不回落 default）。
#[tokio::test(flavor = "current_thread")]
async fn db_flag_targets_declared_profile_and_rejects_unknown() {
    let t = project("dbflag");
    // 声明并迁移：三处瘦身装配的目标库都随 --db 走。模块绑定 test 库（manifest
    // db: 段）——迁移/fixtures/对账只作用于绑定到 --db 所指库的模块。
    std::fs::write(
        t.join("src/m/manifest.yaml"),
        "name: m\ndesc: d\nversion: 0.1.0\ndb: test\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m/schema.yaml"),
        "tables:\n  g:\n    columns:\n      x: { type: text }\n",
    )
    .unwrap();
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: Some("test".into()),
        format: None,
    })
    .await
    .unwrap();
    assert!(
        has_table_in(&t.join("db_test.sqlite"), "g").await,
        "--db test 须作用于 test profile"
    );
    assert!(
        has_table_in(&t.join("db_test.sqlite"), "_oj_migrations").await,
        "账本也须落在 test profile"
    );
    assert!(!has_table(&t, "g").await, "default 库不得被 --db test 写入");
    run_fixture(&FixtureArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        module: None,
        db: Some("test".into()),
    })
    .await
    .unwrap();
    // 对账同样落在 test profile（迁过的库 in sync；default 无绑定模块 → 空对账 in sync）。
    run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: Some("test".into()),
    })
    .await
    .unwrap();
    run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: None,
    })
    .await
    .unwrap();
    // 未声明库名：三处一致 fail-fast 且报出可用键。
    for e in [
        run_migrate(&MigrateArgs {
            site: None,
            config: Some(cfg(&t)),
            dir: src(&t),
            module_root: None,
            baseline: false,
            with_seed: false,
            module: None,
            db: Some("ghost".into()),
            format: None,
        })
        .await
        .unwrap_err(),
        run_fixture(&FixtureArgs {
            site: None,
            config: Some(cfg(&t)),
            dir: src(&t),
            module_root: None,
            module: None,
            db: Some("ghost".into()),
        })
        .await
        .unwrap_err(),
        run_schema_diff(&SchemaDiffArgs {
            config: Some(cfg(&t)),
            dir: src(&t),
            module_root: None,
            db: Some("ghost".into()),
        })
        .await
        .unwrap_err(),
    ] {
        assert!(e.contains("ghost"), "{e}");
        assert!(e.contains("default"), "报错须列出已声明键：{e}");
    }
    let _ = std::fs::remove_dir_all(&t);
}

/// 边界：库名缺失（含空串）一律 fail-fast，绝不回落 default；`db.default` 缺失时
/// `--db` 是唯一出路。
#[tokio::test(flavor = "current_thread")]
async fn db_flag_edge_cases_never_fall_back_to_default() {
    let t = project("dbedge");
    // 空串按「未声明」处理（与 `App::from_config` 同口径），不退化成 default。
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: Some(String::new()),
        format: None,
    })
    .await
    .unwrap_err();
    assert!(e.contains("not declared"), "{e}");
    assert!(
        !has_table(&t, "g").await,
        "空 --db 不得回落 default 执行迁移"
    );
    // 只声明 test（无 default）：未绑定模块无处落账 → 装配期 fail-fast 点名 default。
    let x = format!("sqlite://{}/db_test.sqlite", t.display());
    write_config(&t, &[("test", &x)]);
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap_err();
    assert!(e.contains("default"), "须点名缺失的 default 绑定库：{e}");
    assert!(e.contains("test"), "报错须列出已声明键：{e}");
    assert!(!has_table_in(&t.join("db_test.sqlite"), "g").await);
    // 模块改绑 test 后：--db test 正常迁移（落 test 库，不回落 default）。
    std::fs::write(
        t.join("src/m/manifest.yaml"),
        "name: m\ndesc: d\nversion: 0.1.0\ndb: test\n",
    )
    .unwrap();
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: Some("test".into()),
        format: None,
    })
    .await
    .unwrap();
    assert!(has_table_in(&t.join("db_test.sqlite"), "g").await);
    let _ = std::fs::remove_dir_all(&t);
}

// ===== REQ-HNSW Phase 1：迁移/fixtures/seed/对账按 manifest.db 绑定路由 =====

/// S1：无绑定模块落 default、绑定模块落命名库；同库多模块共存；重跑幂等。
#[tokio::test(flavor = "current_thread")]
async fn migrate_routes_by_manifest_db_binding() {
    let t = project_bound("route");
    // ao2 同绑 vectors：与 ao 同组、都落 vectors。
    std::fs::create_dir_all(t.join("src/ao2/migrations")).unwrap();
    std::fs::write(
        t.join("src/ao2/manifest.yaml"),
        "name: ao2\ndesc: d\nversion: 0.1.0\ndb: vectors\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/ao2/migrations/0001__init.sql"),
        "CREATE TABLE v2 (x);",
    )
    .unwrap();
    run_migrate(&mig_args(&t, None)).await.unwrap();
    assert!(has_table(&t, "g").await, "core（无绑定）落 default");
    assert!(!has_table(&t, "v").await, "ao 不得落 default");
    let dv = t.join("db_vectors.sqlite");
    assert!(
        has_table_in(&dv, "v").await && has_table_in(&dv, "v2").await,
        "ao/ao2 都落 vectors"
    );
    assert!(has_table_in(&dv, "_oj_migrations").await);
    assert_eq!(
        ledger_modules_in(&dv).await,
        vec!["ao".to_string(), "ao2".to_string()]
    );
    // 幂等：重跑账本不增。
    let n = ledger_rows_in(&dv, "ao").await;
    run_migrate(&mig_args(&t, None)).await.unwrap();
    assert_eq!(ledger_rows_in(&dv, "ao").await, n, "重跑不得新增账本行");
    let _ = std::fs::remove_dir_all(&t);
}

/// S2：`--db` 过滤——只跑绑定组，其余库零变更。
#[tokio::test(flavor = "current_thread")]
async fn migrate_db_flag_filters_to_bound_group() {
    let t = project_bound("dbfilter");
    run_migrate(&mig_args(&t, Some("vectors"))).await.unwrap();
    assert!(has_table_in(&t.join("db_vectors.sqlite"), "v").await);
    assert!(
        !has_table(&t, "g").await && !has_table(&t, "_oj_migrations").await,
        "default 库零变更"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S3：绑定库未在 config 声明 → 装配期 fail-fast，点名模块与库名，所有库零变更。
#[tokio::test(flavor = "current_thread")]
async fn migrate_fail_fast_on_undeclared_bound_db() {
    let t = project_bound("undeclared");
    let d = format!("sqlite://{}/db.sqlite", t.display());
    write_config(&t, &[("default", &d)]);
    let e = run_migrate(&mig_args(&t, None)).await.unwrap_err();
    assert!(e.contains("ao") && e.contains("vectors"), "{e}");
    assert!(!has_table(&t, "g").await, "装配期失败不得落任何库");
    let _ = std::fs::remove_dir_all(&t);
}

/// S4：`--db` 指向已声明但无绑定模块的库 → 报错不误跑其它模块。
#[tokio::test(flavor = "current_thread")]
async fn migrate_db_flag_with_no_bound_module_errors() {
    let t = project_bound("nobound");
    let d = format!("sqlite://{}/db.sqlite", t.display());
    let v = format!("sqlite://{}/db_vectors.sqlite", t.display());
    let a = format!("sqlite://{}/db_analytics.sqlite", t.display());
    write_config(&t, &[("default", &d), ("vectors", &v), ("analytics", &a)]);
    let e = run_migrate(&mig_args(&t, Some("analytics")))
        .await
        .unwrap_err();
    assert!(e.contains("analytics"), "{e}");
    assert!(!has_table(&t, "g").await, "不得误跑 default 组");
    let _ = std::fs::remove_dir_all(&t);
}

/// `--module` × `--db`：定向模块绑定他库 → 同 S4 空组报错。
#[tokio::test(flavor = "current_thread")]
async fn migrate_module_flag_with_mismatched_db_errors() {
    let t = project_bound("moddb");
    let e = run_migrate(&MigrateArgs {
        module: Some("core".into()),
        ..mig_args(&t, Some("vectors"))
    })
    .await
    .unwrap_err();
    assert!(e.contains("vectors"), "{e}");
    assert!(!has_table(&t, "g").await);
    let _ = std::fs::remove_dir_all(&t);
}

/// S8：某组部分失败 → 其余组仍应用、整体报错点名失败模块。
#[tokio::test(flavor = "current_thread")]
async fn migrate_partial_failure_continues_other_groups() {
    let t = project_bound("partial");
    std::fs::write(
        t.join("src/ao/migrations/0002__boom.sql"),
        "CREATE TABLE t_syntax_error (",
    )
    .unwrap();
    let e = run_migrate(&mig_args(&t, None)).await.unwrap_err();
    assert!(e.contains("ao"), "{e}");
    assert!(
        has_table(&t, "g").await,
        "default 组不得被 vectors 组失败阻断"
    );
    assert!(
        has_table_in(&t.join("db_vectors.sqlite"), "v").await,
        "ao 的 0001 已提交应保留"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// P0：schema reconcile 随绑定库走（HNSW DDL 的真实承载路径）。
#[tokio::test(flavor = "current_thread")]
async fn migrate_reconcile_targets_bound_db() {
    let t = project_bound("reconcile");
    std::fs::write(
        t.join("src/ao/schema.yaml"),
        "tables:\n  v:\n    columns:\n      x: { type: text }\n      y: { type: text }\n",
    )
    .unwrap();
    run_migrate(&mig_args(&t, None)).await.unwrap();
    let v = connect_db(&t.join("db_vectors.sqlite")).await;
    let cols: Vec<String> = v
        .query("select name from pragma_table_info('v')")
        .await
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        cols.contains(&"y".to_string()),
        "reconcile 加列须落绑定库：{cols:?}"
    );
    let d = t.join("db.sqlite");
    assert!(
        !has_table_in(&d, "v").await,
        "default 库不得被 ao 的 reconcile 触碰"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S-baseline：baseline 记账落绑定库、不执行 SQL。
#[tokio::test(flavor = "current_thread")]
async fn migrate_baseline_targets_bound_db() {
    let t = project_bound("baseline-bound");
    let mut args = mig_args(&t, None);
    args.baseline = true;
    run_migrate(&args).await.unwrap();
    let dv = t.join("db_vectors.sqlite");
    assert!(
        has_table_in(&dv, "_oj_migrations").await,
        "baseline 账本落绑定库"
    );
    assert!(!has_table_in(&dv, "v").await, "baseline 不执行迁移 SQL");
    let _ = std::fs::remove_dir_all(&t);
}

/// S5：`--with-seed` 随绑定库路由——种子行落各模块的绑定库。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_routes_by_bound_db() {
    let t = project_bound("wseed-route");
    std::fs::write(
        t.join("src/core/migrations/0001__init.sql"),
        "CREATE TABLE g (id INTEGER PRIMARY KEY, x TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/ao/migrations/0001__init.sql"),
        "CREATE TABLE v (id INTEGER PRIMARY KEY, x TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/core/seed.sql"),
        "INSERT OR IGNORE INTO g (id, x) VALUES (1, 'c');",
    )
    .unwrap();
    std::fs::write(
        t.join("src/ao/seed.sql"),
        "INSERT OR IGNORE INTO v (id, x) VALUES (1, 'a');",
    )
    .unwrap();
    let mut args = mig_args(&t, None);
    args.with_seed = true;
    run_migrate(&args).await.unwrap();
    let d = connect_db(&t.join("db.sqlite")).await;
    assert_eq!(
        d.query("select x from g").await.unwrap()[0]["x"]
            .as_str()
            .unwrap(),
        "c"
    );
    let v = connect_db(&t.join("db_vectors.sqlite")).await;
    assert_eq!(
        v.query("select x from v").await.unwrap()[0]["x"]
            .as_str()
            .unwrap(),
        "a"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S6：fixture 随绑定库路由。
#[tokio::test(flavor = "current_thread")]
async fn fixture_routes_by_bound_db() {
    let t = project_bound("fx-route");
    run_migrate(&mig_args(&t, None)).await.unwrap();
    std::fs::create_dir_all(t.join("src/core/fixtures")).unwrap();
    std::fs::write(
        t.join("src/core/fixtures/demo.sql"),
        "INSERT OR IGNORE INTO g VALUES (1);",
    )
    .unwrap();
    std::fs::create_dir_all(t.join("src/ao/fixtures")).unwrap();
    std::fs::write(
        t.join("src/ao/fixtures/demo.sql"),
        "INSERT OR IGNORE INTO v VALUES (1);",
    )
    .unwrap();
    run_fixture(&FixtureArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        module: None,
        db: None,
    })
    .await
    .unwrap();
    let d = connect_db(&t.join("db.sqlite")).await;
    assert_eq!(
        d.query("select count(*) as n from g").await.unwrap()[0]["n"]
            .as_i64()
            .unwrap(),
        1
    );
    let v = connect_db(&t.join("db_vectors.sqlite")).await;
    assert_eq!(
        v.query("select count(*) as n from v").await.unwrap()[0]["n"]
            .as_i64()
            .unwrap(),
        1
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S7（行为等价锚）：全部模块无绑定 → 全部落 default，与 v0.1.141 行为一致。
#[tokio::test(flavor = "current_thread")]
async fn migrate_unbound_modules_keep_default_behavior() {
    let t = project("unbound-regression");
    run_migrate(&mig_args(&t, None)).await.unwrap();
    assert!(
        has_table(&t, "g").await && has_table(&t, "_oj_migrations").await,
        "无绑定模块全部落 default"
    );
    assert!(
        !has_table_in(&t.join("db_test.sqlite"), "_oj_migrations").await,
        "test 库零变更"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// --baseline（Q5）：存量库接入门——记账不执行（表不建，账本齐平）。
#[tokio::test(flavor = "current_thread")]
async fn baseline_records_without_executing() {
    let t = project("base");
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: true,
        with_seed: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap();
    assert!(!has_table(&t, "g").await, "baseline 不得执行迁移 SQL");
    assert!(has_table(&t, "_oj_migrations").await, "baseline 必须记账");
    let _ = std::fs::remove_dir_all(&t);
}

/// schema diff：无声明 / 声明与实库一致 → in sync（CI 门禁 Ok 臂）。
#[tokio::test(flavor = "current_thread")]
async fn schema_diff_in_sync_when_decls_match_or_absent() {
    let t = project("diff");
    // 无 schema.yaml：对账空集 → in sync。
    run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: None,
    })
    .await
    .unwrap();
    // 迁移建表后按迁移形状声明（类型不比对，§5.1）→ in sync。
    std::fs::write(
        t.join("src/m/schema.yaml"),
        "tables:\n  g:\n    columns:\n      x: { type: text }\n",
    )
    .unwrap();
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap();
    run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: None,
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&t);
}

/// schema diff：声明了实库没有的表 → D001 漂移 → Err（有差异退 1 的门禁语义）。
#[tokio::test(flavor = "current_thread")]
async fn schema_diff_flags_missing_table_as_drift() {
    let t = project("drift");
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap();
    // 迁移（含 reconcile）之后再补声明 ghost → 实库缺失 → 漂移。
    std::fs::write(
            t.join("src/m/schema.yaml"),
            "tables:\n  g:\n    columns:\n      x: { type: text }\n  ghost:\n    columns:\n      y: { type: text }\n",
        )
        .unwrap();
    let e = run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: None,
    })
    .await
    .unwrap_err();
    // Err 摘要只带计数；明细行（D001 ghost 实库缺失）走 stdout 报告。
    assert!(e.contains("漂移"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// fixtures 无 default 库 → warn 跳过（`oj test` 无 db 项目的共用路径）。
#[tokio::test(flavor = "current_thread")]
async fn fixtures_without_default_db_are_skipped() {
    let n = load_fixtures(None, &[]).await.unwrap();
    assert_eq!(n, 0);
}

/// sqlite 直连夹具（绕开 config/插件装配，只打 load_fixtures 门禁本体）。
async fn sqlite_acc(t: &Path) -> Arc<dyn DataAccessor> {
    only_js::bridge::DbBackendRegistry::builtin()
        .connect(&format!("sqlite://{}", t.join("fx.sqlite").display()), t)
        .await
        .unwrap()
}

/// 幂等门禁（与 S006 seed 同判据）：非幂等 INSERT 执行前拒绝，指回 S006 纪律。
#[tokio::test(flavor = "current_thread")]
async fn given_non_idempotent_fixture_when_load_then_rejected_before_exec() {
    let t = tmpdir("fx-bad");
    let mdir = t.join("m");
    std::fs::create_dir_all(mdir.join("fixtures")).unwrap();
    std::fs::write(mdir.join("fixtures/demo.sql"), "INSERT INTO g VALUES (1);").unwrap();
    let acc = sqlite_acc(&t).await;
    acc.exec_with_params("CREATE TABLE g (x)", &[])
        .await
        .unwrap();
    let e = load_fixtures(Some(&acc), &[("m".into(), mdir)])
        .await
        .unwrap_err();
    assert!(e.contains("非幂等 INSERT"), "{e}");
    assert!(e.contains("S006"), "报错须指回 S006 纪律：{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 幂等写法通过，且重复灌不翻倍（门禁声称的「可重复灌」可观察）。
#[tokio::test(flavor = "current_thread")]
async fn given_idempotent_fixture_when_loaded_twice_then_ok_without_duplicates() {
    let t = tmpdir("fx-ok");
    let mdir = t.join("m");
    std::fs::create_dir_all(mdir.join("fixtures")).unwrap();
    std::fs::write(
        mdir.join("fixtures/demo.sql"),
        "INSERT OR IGNORE INTO g VALUES (1);",
    )
    .unwrap();
    let acc = sqlite_acc(&t).await;
    acc.exec_with_params("CREATE TABLE g (x)", &[])
        .await
        .unwrap();
    let mods = [("m".into(), mdir)];
    let n = load_fixtures(Some(&acc), &mods).await.unwrap();
    assert_eq!(n, 1);
    let n2 = load_fixtures(Some(&acc), &mods).await.unwrap();
    assert_eq!(n2, 1, "重灌应同样通过（幂等）");
    let rows = acc.query("select count(*) from g").await.unwrap();
    assert_eq!(rows.len(), 1, "重复灌不得翻倍：{rows:?}");
    let _ = std::fs::remove_dir_all(&t);
}

// ===== 阶段 6 新增：`--format json` / sanitize_dsn / `<module>-<version>` 定向 =====

/// (模块名, 版本, 迁移文件[(文件名, SQL)])。
type DistMod<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

/// 构建 release（dist）树：`<t>/dist/manifests.yaml` 记录 name→version，并为每个
/// 模块建 `<t>/dist/<name>-<version>/migrations/<file>`（migs 可为空 → 该模块无迁移）。
fn dist_tree(t: &Path, mods: &[DistMod]) {
    let dist = t.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    let mut lock = String::new();
    for (name, ver, migs) in mods {
        lock.push_str(&format!("{name}: {ver}\n"));
        let mdir = dist.join(format!("{name}-{ver}"));
        std::fs::create_dir_all(&mdir).unwrap();
        // 复刻真实 oj build 产物：模块目录含 manifest.yaml（无 db: 绑定 → default）。
        std::fs::write(
            mdir.join("manifest.yaml"),
            format!("name: {name}\ndesc: d\nversion: {ver}\n"),
        )
        .unwrap();
        let mg = mdir.join("migrations");
        std::fs::create_dir_all(&mg).unwrap();
        for (fname, sql) in *migs {
            std::fs::write(mg.join(fname), sql).unwrap();
        }
    }
    std::fs::write(dist.join("manifests.yaml"), lock).unwrap();
}

/// 直连某 sqlite 文件（目标库），供账本/表断言。
async fn connect_db(db: &Path) -> Arc<dyn DataAccessor> {
    only_js::bridge::DbBackendRegistry::builtin()
        .connect(&format!("sqlite://{}", db.display()), db.parent().unwrap())
        .await
        .unwrap()
}

/// 账本中出现的全部 distinct module（按字母序）。
async fn ledger_modules_in(db: &Path) -> Vec<String> {
    connect_db(db)
        .await
        .query("select distinct module from _oj_migrations order by module")
        .await
        .unwrap()
        .iter()
        .map(|r| r["module"].as_str().unwrap().to_string())
        .collect()
}

/// 账本中某 module 的行数（去重计数，验证幂等）。
async fn ledger_rows_in(db: &Path, module: &str) -> usize {
    connect_db(db)
        .await
        .query(&format!(
            "select count(*) as n from _oj_migrations where module = '{module}'"
        ))
        .await
        .unwrap()[0]["n"]
        .as_i64()
        .unwrap() as usize
}

/// S-sanitize-1：DSN 凭据段抹除（host/端口保留）；无 `@` 权威段原样；不限 scheme。
#[test]
fn s_sanitize_dsn_redacts_credentials() {
    assert_eq!(
        sanitize_dsn("mysql://root:s3cr3t@db:3306/app"),
        "mysql://***@db:3306/app"
    );
    assert_eq!(sanitize_dsn("no dsn here"), "no dsn here");
    assert_eq!(
        sanitize_dsn("postgres://u:pw@host/db"),
        "postgres://***@host/db"
    );
}

/// S-json-1：双模块空库（无迁移）→ json 模式 `Ok(())`（total 0，退出码 0 语义）。
#[tokio::test(flavor = "current_thread")]
async fn s_json_two_empty_modules_ok() {
    let t = project("json-empty");
    dist_tree(&t, &[("modA", "1.0.0", &[]), ("modB", "1.0.0", &[])]);
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: Some(t.join("dist").display().to_string()),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&t);
}

/// S-json-2：A 合法、B 语法错 → json 模式 `Err` 含 B 名；A 已迁移、B 未迁移
/// （continue 且 A 不回滚——模块间独立事务）。
#[tokio::test(flavor = "current_thread")]
async fn s_json_continue_on_module_failure() {
    let t = project("json-cont");
    dist_tree(
        &t,
        &[
            ("modA", "1.0.0", &[("0001__a.sql", "CREATE TABLE ta (x);")]),
            (
                "modB",
                "1.0.0",
                &[("0001__b.sql", "CREATE TABLE tb (x) GARBAGE;")],
            ),
        ],
    );
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: Some(t.join("dist").display().to_string()),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap_err();
    assert!(e.contains("modB"), "Err 须点名失败模块：{e}");
    // A 已迁移、B 未迁移——证明 continue 且 A 不回滚。
    assert_eq!(
        ledger_modules_in(&t.join("db.sqlite")).await,
        vec!["modA".to_string()],
        "失败的 B 不得污染账本"
    );
    assert!(
        has_table_in(&t.join("db.sqlite"), "ta").await,
        "A 的表须建好"
    );
    assert!(
        !has_table_in(&t.join("db.sqlite"), "tb").await,
        "B 不得建表（失败回滚）"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S-json-3：全量跑两次 → 第二次 `Ok` 且账本去重（applied 为空，由 e2e 覆盖 stdout）。
#[tokio::test(flavor = "current_thread")]
async fn s_json_idempotent_rerun() {
    let t = project("json-idem");
    dist_tree(
        &t,
        &[("modA", "1.0.0", &[("0001__a.sql", "CREATE TABLE ta (x);")])],
    );
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: Some(t.join("dist").display().to_string()),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    assert_eq!(ledger_rows_in(&t.join("db.sqlite"), "modA").await, 1);
    // 第二次：仍 `Ok`，且账本行数不变（去重，无重复应用）。
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: Some(t.join("dist").display().to_string()),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    assert_eq!(ledger_rows_in(&t.join("db.sqlite"), "modA").await, 1);
    let _ = std::fs::remove_dir_all(&t);
}

/// S-json-4：失败位点——模块首个迁移语法错（账本空）→ 错误含 `at seq 1:`。
/// 走真实 json 输出路径：直接调 `migrate_collect` + `json_line`（避免捕获 stdout）。
#[tokio::test(flavor = "current_thread")]
async fn s_json_failure_site_seq() {
    let t = tmpdir("json-seq");
    let mdir = t.join("modX");
    std::fs::create_dir_all(mdir.join("migrations")).unwrap();
    std::fs::write(
        mdir.join("migrations/0001__bad.sql"),
        "CREATE TABLE tx (x) GARBAGE;",
    )
    .unwrap();
    let acc = connect_db(&t.join("x.sqlite")).await;
    let modules = vec![("modX".to_string(), mdir)];
    let (results, _total) =
        migrate_collect(&acc, &modules, false, only_js::bridge::SqlGuard::Off, true).await;
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok, "该模块须标记失败");
    let out = JsonOut {
        results,
        error: None,
        seed: None,
    };
    let line = json_line(&out).unwrap();
    assert!(
        line.contains("at seq 1:"),
        "json error 须含失败位点：{line}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S-dirversion-5（版本目录名定向）：`--module modA-1.0.0` → 仅 modA 被迁移。
#[tokio::test(flavor = "current_thread")]
async fn s_dirversion_targets_named_version_dir() {
    let t = project("dirver");
    dist_tree(
        &t,
        &[
            ("modA", "1.0.0", &[("0001__a.sql", "CREATE TABLE ta (x);")]),
            ("modB", "1.0.0", &[("0001__b.sql", "CREATE TABLE tb (x);")]),
        ],
    );
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: Some(t.join("dist").display().to_string()),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: Some("modA-1.0.0".into()),
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    assert_eq!(
        ledger_modules_in(&t.join("db.sqlite")).await,
        vec!["modA".to_string()],
        "版本目录名定向须只迁移 modA"
    );
    assert!(has_table_in(&t.join("db.sqlite"), "ta").await);
    assert!(!has_table_in(&t.join("db.sqlite"), "tb").await);
    let _ = std::fs::remove_dir_all(&t);
}

/// S-dirversion-5（裸名定向）：`--module modA` → 仅 modA 被迁移（与版本目录名等价）。
#[tokio::test(flavor = "current_thread")]
async fn s_dirversion_bare_name_targets_only_module() {
    let t = project("dirver2");
    dist_tree(
        &t,
        &[
            ("modA", "1.0.0", &[("0001__a.sql", "CREATE TABLE ta (x);")]),
            ("modB", "1.0.0", &[("0001__b.sql", "CREATE TABLE tb (x);")]),
        ],
    );
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: Some(t.join("dist").display().to_string()),
        module_root: None,
        baseline: false,
        with_seed: false,
        module: Some("modA".into()),
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    assert_eq!(
        ledger_modules_in(&t.join("db.sqlite")).await,
        vec!["modA".to_string()],
        "裸名定向须只迁移 modA"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// S-unsupported-format：`--format xml` → `Err` 点名 unsupported（`slim` 前即失败）。
#[tokio::test(flavor = "current_thread")]
async fn s_unsupported_format_rejected() {
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: None,
        dir: None,
        module_root: None,
        baseline: false,
        with_seed: false,
        module: None,
        db: None,
        format: Some("xml".into()),
    })
    .await
    .unwrap_err();
    assert!(e.contains("unsupported"), "须点名 unsupported：{e}");
}

// ===== `--with-seed`（v0.1.125，spec 2026-11-07）================================

/// CLI 全链路（§9 #1 的 oj-bin 等价验证）：`run_migrate --with-seed` 同一次调用、
/// 不重启，迁移建表 + seed 灌入即得种子行；重跑幂等不翻倍。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_replays_after_migrate_and_is_idempotent() {
    let t = project("wseed-ok");
    std::fs::write(
        t.join("src/m/migrations/0001__init.sql"),
        "CREATE TABLE g (id INTEGER PRIMARY KEY, x TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m/seed.sql"),
        "INSERT OR IGNORE INTO g (id, x) VALUES (1, 'neo');",
    )
    .unwrap();
    let args = || MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: true,
        module: None,
        db: None,
        format: Some("json".into()),
    };
    run_migrate(&args()).await.unwrap();
    let acc = connect_db(&t.join("db.sqlite")).await;
    let n = || async {
        acc.query("select count(*) as n from g").await.unwrap()[0]["n"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(n().await, 1, "seed 须随 migrate 同批灌入、不重启即得行");
    // 重跑幂等：行数不翻倍（§9 #2）。
    run_migrate(&args()).await.unwrap();
    assert_eq!(n().await, 1, "重复 --with-seed 不得翻倍");
    let _ = std::fs::remove_dir_all(&t);
}

/// 门禁击穿（§9 #7）：seed 含 DDL → 整条命令非零退出、全量预扫零副作用；
/// 迁移本身不受影响（迁移门禁先于 seed）。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_rejects_ddl_with_zero_side_effects() {
    let t = project("wseed-ddl");
    std::fs::write(t.join("src/m/seed.sql"), "/*x*/ CREATE TABLE injected (x);").unwrap();
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: true,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap_err();
    assert!(
        e.contains("--with-seed"),
        "Exit 非 0，原因须点名 --with-seed：{e}"
    );
    assert!(has_table(&t, "g").await, "迁移照常成功建 g");
    assert!(
        !has_table(&t, "injected").await,
        "DDL 不得落库（全量预扫零副作用）"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// 不带旗标：JSON 逐字不变（无 `seed` 键，§9 #5）；带旗标：顶层 `seed.results[]`
/// 出现，且 `file` 只出文件名、不含绝对路径（§6.8）。
#[test]
fn seed_json_field_absent_without_flag_present_with() {
    let no = json_line(&JsonOut {
        results: Vec::new(),
        error: None,
        seed: None,
    })
    .unwrap();
    assert!(!no.contains("seed"), "不带旗标 JSON 不得出现 seed 键：{no}");
    let yes = json_line(&JsonOut {
        results: Vec::new(),
        error: None,
        seed: Some(SeedJson {
            results: vec![crate::seed::SeedResult {
                module: "m".into(),
                ok: true,
                file: Some("seed.sql".into()),
                dialect: Some("sqlite".into()),
                stmts: Some(1),
                error: None,
            }],
            error: None,
        }),
    })
    .unwrap();
    assert!(yes.contains("\"seed\":{"), "带旗标须出现顶层 seed：{yes}");
    assert!(yes.contains("seed.sql"), "{yes}");
    assert!(
        !yes.contains("src/m/seed.sql"),
        "file 不得含绝对路径：{yes}"
    );
}

/// 语义钉死（用户 2026-11-07）：「单独模块只执行本模块 sql」——seed 作用域严格 =
/// `s.modules`，每个模块只在自己的模块根下 `pick` 本模块的 seed.sql，不整树扩散、
/// 不跨模块串台、不递归子目录。两个模块各写各的 seed，全量 `--with-seed` 后各表
/// 各得自己的一行；模块内 nested/ 里的 seed.sql 不得被 scan 到。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_scope_is_per_module_no_cross_talk() {
    let t = project("wseed-scope");
    std::fs::write(
        t.join("src/m/migrations/0001__init.sql"),
        "CREATE TABLE g (id INTEGER PRIMARY KEY, tag TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m/seed.sql"),
        "INSERT OR IGNORE INTO g (id, tag) VALUES (1, 'from-m');",
    )
    .unwrap();
    // 模块 m2：独立的迁移 + 独立的 seed。
    std::fs::create_dir_all(t.join("src/m2/migrations")).unwrap();
    std::fs::write(
        t.join("src/m2/manifest.yaml"),
        "name: m2\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m2/migrations/0001__init.sql"),
        "CREATE TABLE h (id INTEGER PRIMARY KEY, tag TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m2/seed.sql"),
        "INSERT OR IGNORE INTO h (id, tag) VALUES (1, 'from-m2');",
    )
    .unwrap();
    // 模块内子目录里再放一份 seed.sql：scan 只读模块根首层文件，不得递归执行。
    std::fs::create_dir_all(t.join("src/m/nested")).unwrap();
    std::fs::write(
        t.join("src/m/nested/seed.sql"),
        "INSERT OR IGNORE INTO g (id, tag) VALUES (9, 'nested');",
    )
    .unwrap();
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: true,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    let acc = connect_db(&t.join("db.sqlite")).await;
    let g = acc.query("select tag from g").await.unwrap();
    let h = acc.query("select tag from h").await.unwrap();
    assert_eq!(g.len(), 1, "g 只应有 m 自己的一行：{g:?}");
    assert_eq!(g[0]["tag"].as_str(), Some("from-m"));
    assert_eq!(h.len(), 1, "h 只应有 m2 自己的一行：{h:?}");
    assert_eq!(h[0]["tag"].as_str(), Some("from-m2"));
    let _ = std::fs::remove_dir_all(&t);
}

/// `--module` 定向：seed 作用域跟随 `--module` 过滤后的 `s.modules`，不扩散到未入选
/// 的模块（设计 §5 组合语义 + 阶段 3「模块定向」待办）。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_module_filter_scopes_seed() {
    let t = project("wseed-filter");
    std::fs::write(
        t.join("src/m/migrations/0001__init.sql"),
        "CREATE TABLE g (id INTEGER PRIMARY KEY, tag TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m/seed.sql"),
        "INSERT OR IGNORE INTO g VALUES (1, 'from-m');",
    )
    .unwrap();
    std::fs::create_dir_all(t.join("src/m2/migrations")).unwrap();
    std::fs::write(
        t.join("src/m2/manifest.yaml"),
        "name: m2\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m2/migrations/0001__init.sql"),
        "CREATE TABLE h (id INTEGER PRIMARY KEY, tag TEXT);",
    )
    .unwrap();
    std::fs::write(
        t.join("src/m2/seed.sql"),
        "INSERT OR IGNORE INTO h VALUES (1, 'from-m2');",
    )
    .unwrap();
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: true,
        module: Some("m2".into()),
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap();
    let acc = connect_db(&t.join("db.sqlite")).await;
    let h = acc.query("select count(*) as n from h").await.unwrap();
    assert_eq!(h[0]["n"].as_i64().unwrap(), 1, "m2 的 seed 须随迁移生效");
    assert!(
        !has_table(&t, "g").await,
        "--module m2 时 m 不在 s.modules，其 seed 不得被扩散执行"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// 组合语义（设计 §5）：migrate 有模块失败 → 不跑 seed，整条命令失败。预建 g 表，
/// 使「万一 seed 被错误执行」会成功插入一行——以此反证 seed 确实没跑。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_migrate_failure_skips_seed() {
    let t = project("wseed-migfail");
    std::fs::write(
        t.join("src/m/migrations/0001__init.sql"),
        "CREATE TABLE zz (x) GARBAGE;",
    )
    .unwrap();
    let acc = connect_db(&t.join("db.sqlite")).await;
    acc.exec_with_params("CREATE TABLE g (x)", &[])
        .await
        .unwrap();
    std::fs::write(
        t.join("src/m/seed.sql"),
        "INSERT OR IGNORE INTO g VALUES (1);",
    )
    .unwrap();
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: true,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap_err();
    assert!(!e.contains("--with-seed"), "失败须来自迁移、不是 seed：{e}");
    let n = acc.query("select count(*) as n from g").await.unwrap();
    assert_eq!(n[0]["n"].as_i64().unwrap(), 0, "迁移失败时 seed 一律不跑");
    let _ = std::fs::remove_dir_all(&t);
}

/// FR5 / §8：seed **执行期**失败（门禁放行的幂等 INSERT 打到不存在的表）也要让整条
/// 命令非零退出——覆盖 `run_seed_phase` 的 `Ok` 分支里逐模块 `ok=false` 的折返路径
/// （与 DDL 被门禁全局拒绝的 `Err` 分支不同）。
#[tokio::test(flavor = "current_thread")]
async fn with_seed_exec_failure_is_nonzero() {
    let t = project("wseed-execfail");
    std::fs::write(
        t.join("src/m/seed.sql"),
        "INSERT OR IGNORE INTO missing_table VALUES (1);",
    )
    .unwrap();
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        with_seed: true,
        module: None,
        db: None,
        format: Some("json".into()),
    })
    .await
    .unwrap_err();
    assert!(e.contains("--with-seed"), "seed 失败须整体非零退出：{e}");
    assert!(has_table(&t, "g").await, "迁移本身应照常成功");
    let _ = std::fs::remove_dir_all(&t);
}
