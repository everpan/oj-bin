//! `oj migrate` / `oj fixture`（§4.6）：**瘦身装配**——不走 `App::from_config`
//! （其证书门禁无逃生口、且携带 seed/路由），CI/运维机无证书也可执行迁移；
//! 只解析 config → 插件 → 逐 db 开库 → 迁移/fixtures。

use std::path::PathBuf;
use std::sync::Arc;

use only_js::bridge::DataAccessor;

use crate::args::{FixtureArgs, MigrateArgs, SchemaDiffArgs};
use crate::server_cmd::{Registries, assemble_plugins, connect_dbs, load_app_config};

/// 瘦身装配产物：default 库句柄 + 模块列表（可被 `--module` 过滤）。
struct Slim {
    default: Arc<dyn DataAccessor>,
    modules: Vec<(String, PathBuf)>,
}

async fn slim(
    config: &str,
    dir_override: Option<&str>,
    module: Option<&str>,
) -> Result<Slim, String> {
    let (cfg, config_dir, dir, ts, _base) = load_app_config(config, dir_override, None)?;
    let mut registries = Registries::default();
    assemble_plugins(&cfg, &config_dir, &mut registries).await?;
    let dbs = connect_dbs(&cfg.db, &registries.dbs, &config_dir).await?;
    let default = dbs
        .get("default")
        .ok_or("config has no 'default' db（迁移/fixtures 作用于 default 库）")?
        .clone();
    let mut modules = crate::manifest::discover(&dir, ts)?;
    if let Some(m) = module {
        crate::manifest::validate_module(m)?;
        if !modules.iter().any(|(n, _)| n == m) {
            return Err(format!("module {m:?} not found under {}", dir.display()));
        }
        modules.retain(|(n, _)| n == m);
    }
    Ok(Slim { default, modules })
}

/// `oj migrate [-c config] [-d dir] [--baseline] [--module M]`：
/// 逐模块载入 migrations/ 并 apply 到最新；`--baseline` = 全量记账不执行
/// （P0 建过表的存量库接入门，Q5）。
pub async fn run_migrate(a: &MigrateArgs) -> Result<(), String> {
    let s = slim(&a.config, a.dir.as_deref(), a.module.as_deref()).await?;
    let mut total = 0;
    for (name, mdir) in &s.modules {
        let n = crate::migrate::apply_module(&s.default, name, mdir, a.baseline).await?;
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
            for l in crate::schema::reconcile(s.default.as_ref(), name, &f).await? {
                println!("oj migrate: {l}");
            }
        }
    }
    println!(
        "oj migrate: {total} migration(s) across {} module(s)",
        s.modules.len()
    );
    Ok(())
}

/// `oj fixture [-c config] [-d dir] [--module M]`：灌 fixtures/ 演示数据（§4.5）。
pub async fn run_fixture(a: &FixtureArgs) -> Result<(), String> {
    let s = slim(&a.config, a.dir.as_deref(), a.module.as_deref()).await?;
    let n = load_fixtures(Some(&s.default), &s.modules).await?;
    println!("oj fixture: {n} statement(s) loaded");
    Ok(())
}

/// `oj schema diff [-c config] [-d dir]`：声明 vs 实库只读对账（D001/D002，§5.1）。
/// 有差异 → 打印报告并 Err（进程退 1，CI 门禁可用）；一致 → in sync。
pub async fn run_schema_diff(a: &SchemaDiffArgs) -> Result<(), String> {
    let s = slim(&a.config, a.dir.as_deref(), None).await?;
    let mut mods = Vec::new();
    for (name, mdir) in &s.modules {
        if let Some(f) = crate::schema::SchemaFile::load(mdir)? {
            mods.push((name.clone(), f));
        }
    }
    let report = crate::schema::diff(s.default.as_ref(), &mods).await?;
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

/// fixtures/*.sql（演示数据）：按文件名排序、`;` 朴素切分，exec 到 default 库。
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
            for stmt in t.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                acc.exec_with_params(stmt, &[])
                    .await
                    .map_err(|e| format!("fixture {name}/{}: {e}", f.display()))?;
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

    /// 夹具：项目根（config.yaml + src/m/{manifest,migrations,fixtures}）。
    fn project(tag: &str) -> PathBuf {
        let t = tmpdir(tag);
        std::fs::write(
            t.join("config.yaml"),
            format!("db:\n  default: sqlite://{}/db.sqlite\n", t.display()),
        )
        .unwrap();
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

    async fn has_table(t: &Path, name: &str) -> bool {
        let acc = only_js::bridge::DbBackendRegistry::builtin()
            .connect(&format!("sqlite://{}/db.sqlite", t.display()), t)
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

    /// CLI 全链路：run_migrate 建表 + 账本；重跑幂等；run_fixture 灌数。
    #[tokio::test(flavor = "current_thread")]
    async fn migrate_and_fixture_end_to_end() {
        let t = project("e2e");
        run_migrate(&MigrateArgs {
            config: t.join("config.yaml").display().to_string(),
            dir: Some(t.join("src").display().to_string()),
            baseline: false,
            module: None,
        })
        .await
        .unwrap();
        assert!(has_table(&t, "g").await);
        assert!(has_table(&t, "_oj_migrations_m").await);
        // 幂等：重跑不增不改。
        run_migrate(&MigrateArgs {
            config: t.join("config.yaml").display().to_string(),
            dir: Some(t.join("src").display().to_string()),
            baseline: false,
            module: None,
        })
        .await
        .unwrap();
        // fixture：演示数据进表（不进账本）。
        run_fixture(&FixtureArgs {
            config: t.join("config.yaml").display().to_string(),
            dir: Some(t.join("src").display().to_string()),
            module: None,
        })
        .await
        .unwrap();
        // 未知模块 fail-fast。
        let e = run_migrate(&MigrateArgs {
            config: t.join("config.yaml").display().to_string(),
            dir: Some(t.join("src").display().to_string()),
            baseline: false,
            module: Some("ghost".into()),
        })
        .await
        .unwrap_err();
        assert!(e.contains("ghost"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// --baseline（Q5）：存量库接入门——记账不执行（表不建，账本齐平）。
    #[tokio::test(flavor = "current_thread")]
    async fn baseline_records_without_executing() {
        let t = project("base");
        run_migrate(&MigrateArgs {
            config: t.join("config.yaml").display().to_string(),
            dir: Some(t.join("src").display().to_string()),
            baseline: true,
            module: None,
        })
        .await
        .unwrap();
        assert!(!has_table(&t, "g").await, "baseline 不得执行迁移 SQL");
        assert!(has_table(&t, "_oj_migrations_m").await, "baseline 必须记账");
        let _ = std::fs::remove_dir_all(&t);
    }
}
