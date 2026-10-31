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
    // 声明并迁移：三处瘦身装配的目标库都随 --db 走。
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
    // 对账同样落在 test profile（迁过的库 in sync；default 未迁 → D001 漂移）。
    run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: Some("test".into()),
    })
    .await
    .unwrap();
    let e = run_schema_diff(&SchemaDiffArgs {
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        db: None,
    })
    .await
    .unwrap_err();
    assert!(e.contains("漂移"), "--db 缺省仍对 default 库对账：{e}");
    // 未声明库名：三处一致 fail-fast 且报出可用键。
    for e in [
        run_migrate(&MigrateArgs {
            site: None,
            config: Some(cfg(&t)),
            dir: src(&t),
            module_root: None,
            baseline: false,
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
    // 只声明 test（无 default）：不给 --db 报错并指路，给 --db 则正常迁移。
    let x = format!("sqlite://{}/db_test.sqlite", t.display());
    write_config(&t, &[("test", &x)]);
    let e = run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        module: None,
        db: None,
        format: None,
    })
    .await
    .unwrap_err();
    assert!(e.contains("--db"), "缺 default 须指路 --db：{e}");
    assert!(!has_table_in(&t.join("db_test.sqlite"), "g").await);
    run_migrate(&MigrateArgs {
        site: None,
        config: Some(cfg(&t)),
        dir: src(&t),
        module_root: None,
        baseline: false,
        module: None,
        db: Some("test".into()),
        format: None,
    })
    .await
    .unwrap();
    assert!(has_table_in(&t.join("db_test.sqlite"), "g").await);
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
        let mg = dist.join(format!("{name}-{ver}")).join("migrations");
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
        module: None,
        db: None,
        format: Some("xml".into()),
    })
    .await
    .unwrap_err();
    assert!(e.contains("unsupported"), "须点名 unsupported：{e}");
}
