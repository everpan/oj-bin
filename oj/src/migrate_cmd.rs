//! `oj migrate` / `oj fixture`（§4.6）：**瘦身装配**——不走 `App::from_config`
//! （其证书门禁无逃生口、且携带 seed/路由），CI/运维机无证书也可执行迁移；
//! 只解析 config → 插件 → 逐 db 开库 → 迁移/fixtures。

use std::path::PathBuf;
use std::sync::Arc;

use only_js::bridge::DataAccessor;

use crate::args::{FixtureArgs, MigrateArgs, SchemaDiffArgs};
use crate::server_cmd::{Registries, assemble_plugins, connect_dbs, load_app_config};

/// 瘦身装配产物：目标库句柄（`--db` 选定的 config profile）+ 模块列表（可被 `--module` 过滤）。
struct Slim {
    target: Arc<dyn DataAccessor>,
    modules: Vec<(String, PathBuf)>,
    sql_guard: only_js::bridge::SqlGuard,
}

async fn slim(
    config: &str,
    dir_override: Option<&str>,
    module: Option<&str>,
    db: Option<&str>,
) -> Result<Slim, String> {
    let (cfg, config_dir, dir, ts, _base) = load_app_config(config, dir_override, None)?;
    // 迁移/fixtures 作用于 api 目录下的 SQL；强依赖 api 目录（无「纯静态」形态）。
    if !dir.is_dir() {
        return Err(format!(
            "service dir not found: {}（src 源码树或 oj build 产物 dist）",
            dir.display()
        ));
    }
    let mut registries = Registries::default();
    assemble_plugins(&cfg, &config_dir, &mut registries).await?;
    let dbs = connect_dbs(&cfg.db, &registries.dbs, &config_dir).await?;
    // 目标库（v0.1.21）：`--db` 选 config `db:` 段的 profile，缺省 "default"。
    // 未声明的库名 fail-fast——静默回落 default 等于把迁移打在开发库上（与
    // `oj test --db`、`App::from_config` 同一条纪律）。
    let key = db.unwrap_or("default");
    let target = dbs
        .get(key)
        .ok_or_else(|| {
            let mut names: Vec<&str> = dbs.keys().map(|s| s.as_str()).collect();
            names.sort_unstable();
            match db {
                Some(_) => format!("--db {key:?} not declared in config (db keys: {names:?})"),
                None => format!(
                    "config has no 'default' db（迁移/fixtures/对账需要一个目标库；\
                     用 --db <name> 指定其它 profile，已声明：{names:?}）"
                ),
            }
        })?
        .clone();
    tracing::info!(db = key, "target db selected");
    let mut modules = crate::manifest::discover(&dir, ts)?;
    if let Some(m) = module {
        crate::manifest::validate_module(m)?;
        if !modules.iter().any(|(n, _)| n == m) {
            return Err(format!("module {m:?} not found under {}", dir.display()));
        }
        modules.retain(|(n, _)| n == m);
    }
    Ok(Slim {
        target,
        modules,
        sql_guard: cfg.tenant.sql_guard,
    })
}

/// `oj migrate [-c config] [-d dir] [--db name] [--baseline] [--module M]`：
/// 逐模块载入 migrations/ 并 apply 到最新；`--baseline` = 全量记账不执行
/// （P0 建过表的存量库接入门，Q5）；`--db` = 目标库 profile（缺省 default）。
pub async fn run_migrate(a: &MigrateArgs) -> Result<(), String> {
    let s = slim(
        &a.config,
        a.dir.as_deref(),
        a.module.as_deref(),
        a.db.as_deref(),
    )
    .await?;
    let mut total = 0;
    for (name, mdir) in &s.modules {
        let n = crate::migrate::apply_module(&s.target, name, mdir, a.baseline).await?;
        if n > 0 {
            println!(
                "oj migrate: {name}: {n} applied{}",
                if a.baseline {
                    " (baseline, recorded only)"
                } else {
                    ""
                }
            );
        }
        total += n;
        // schema.yaml 安全前向收敛（§D1：reconcile 只进 apply 路径，迁移后补声明漂移）。
        if let Some(f) = crate::schema::SchemaFile::load(mdir)? {
            if s.sql_guard != only_js::bridge::SqlGuard::Off {
                f.validate_tenant(name)?;
            }
            for l in crate::schema::reconcile(s.target.as_ref(), name, &f).await? {
                println!("oj migrate: {l}");
            }
        }
    }
    println!(
        "oj migrate: {total} migration(s) across {} module(s) → db {:?}",
        s.modules.len(),
        a.db.as_deref().unwrap_or("default")
    );
    Ok(())
}

/// `oj fixture [-c config] [-d dir] [--db name] [--module M]`：灌 fixtures/ 演示数据（§4.5）。
pub async fn run_fixture(a: &FixtureArgs) -> Result<(), String> {
    let s = slim(
        &a.config,
        a.dir.as_deref(),
        a.module.as_deref(),
        a.db.as_deref(),
    )
    .await?;
    let n = load_fixtures(Some(&s.target), &s.modules).await?;
    println!("oj fixture: {n} statement(s) loaded");
    Ok(())
}

/// `oj schema diff [-c config] [-d dir] [--db name]`：声明 vs 实库只读对账（D001/D002，§5.1）。
/// 有差异 → 打印报告并 Err（进程退 1，CI 门禁可用）；一致 → in sync。
pub async fn run_schema_diff(a: &SchemaDiffArgs) -> Result<(), String> {
    let s = slim(&a.config, a.dir.as_deref(), None, a.db.as_deref()).await?;
    let mut mods = Vec::new();
    for (name, mdir) in &s.modules {
        if let Some(f) = crate::schema::SchemaFile::load(mdir)? {
            mods.push((name.clone(), f));
        }
    }
    let report = crate::schema::diff(s.target.as_ref(), &mods).await?;
    if report.is_empty() {
        println!(
            "oj schema diff: in sync（{} 个模块声明与实库一致）",
            mods.len()
        );
        Ok(())
    } else {
        for l in &report {
            println!("{l}");
        }
        Err(format!(
            "{} 处漂移（只读报告，oj migrate 可收敛安全前向）",
            report.len()
        ))
    }
}

/// fixtures/*.sql（演示数据）：按文件名排序、`;` 朴素切分，exec 到目标库。
/// 不记账本——幂等演示数据可重复灌。供 `oj fixture` 与 `oj test`
/// （from_config fixtures=true）共用。
pub async fn load_fixtures(
    default: Option<&Arc<dyn DataAccessor>>,
    modules: &[(String, PathBuf)],
) -> Result<usize, String> {
    let Some(acc) = default else {
        eprintln!("warn: fixtures skipped (no default db)");
        return Ok(0);
    };
    let mut n = 0;
    for (name, mdir) in modules {
        let Ok(rd) = std::fs::read_dir(mdir.join("fixtures")) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        for f in files {
            let t =
                std::fs::read_to_string(&f).map_err(|e| format!("read {}: {e}", f.display()))?;
            for (i, stmt) in t
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .enumerate()
            {
                match acc.exec_with_params(stmt, &[]).await {
                    Ok(rows) => tracing::info!(
                        module = name,
                        file = %f.display(),
                        seq = i,
                        rows,
                        stmt = %crate::migrate::log_snip(stmt),
                        "fixture ok"
                    ),
                    Err(e) => {
                        tracing::error!(
                            module = name,
                            file = %f.display(),
                            seq = i,
                            stmt = %crate::migrate::log_snip(stmt),
                            "fixture failed: {e}"
                        );
                        return Err(format!("fixture {name}/{}: {e}", f.display()));
                    }
                }
                n += 1;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
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
            "INSERT INTO g VALUES (1);",
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
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: None,
        })
        .await
        .unwrap();
        assert!(has_table(&t, "g").await);
        assert!(has_table(&t, "_oj_migrations").await);
        // 幂等：重跑不增不改。
        run_migrate(&MigrateArgs {
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: None,
        })
        .await
        .unwrap();
        // fixture：演示数据进表（不进账本）。
        run_fixture(&FixtureArgs {
            config: cfg(&t),
            dir: src(&t),
            module: None,
            db: None,
        })
        .await
        .unwrap();
        // 未知模块 fail-fast。
        let e = run_migrate(&MigrateArgs {
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: Some("ghost".into()),
            db: None,
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
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: Some("test".into()),
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
            config: cfg(&t),
            dir: src(&t),
            module: None,
            db: Some("test".into()),
        })
        .await
        .unwrap();
        // 对账同样落在 test profile（迁过的库 in sync；default 未迁 → D001 漂移）。
        run_schema_diff(&SchemaDiffArgs {
            config: cfg(&t),
            dir: src(&t),
            db: Some("test".into()),
        })
        .await
        .unwrap();
        let e = run_schema_diff(&SchemaDiffArgs {
            config: cfg(&t),
            dir: src(&t),
            db: None,
        })
        .await
        .unwrap_err();
        assert!(e.contains("漂移"), "--db 缺省仍对 default 库对账：{e}");
        // 未声明库名：三处一致 fail-fast 且报出可用键。
        for e in [
            run_migrate(&MigrateArgs {
                config: cfg(&t),
                dir: src(&t),
                baseline: false,
                module: None,
                db: Some("ghost".into()),
            })
            .await
            .unwrap_err(),
            run_fixture(&FixtureArgs {
                config: cfg(&t),
                dir: src(&t),
                module: None,
                db: Some("ghost".into()),
            })
            .await
            .unwrap_err(),
            run_schema_diff(&SchemaDiffArgs {
                config: cfg(&t),
                dir: src(&t),
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
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: Some(String::new()),
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
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: None,
        })
        .await
        .unwrap_err();
        assert!(e.contains("--db"), "缺 default 须指路 --db：{e}");
        assert!(!has_table_in(&t.join("db_test.sqlite"), "g").await);
        run_migrate(&MigrateArgs {
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: Some("test".into()),
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
            config: cfg(&t),
            dir: src(&t),
            baseline: true,
            module: None,
            db: None,
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
            config: cfg(&t),
            dir: src(&t),
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
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: None,
        })
        .await
        .unwrap();
        run_schema_diff(&SchemaDiffArgs {
            config: cfg(&t),
            dir: src(&t),
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
            config: cfg(&t),
            dir: src(&t),
            baseline: false,
            module: None,
            db: None,
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
            config: cfg(&t),
            dir: src(&t),
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
}
