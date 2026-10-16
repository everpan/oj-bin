//! `oj migrate` / `oj test fixture`（§4.6）：**瘦身装配**——不走 `App::from_config`
//! （其证书门禁无逃生口、且携带 seed/路由），CI/运维机无证书也可执行迁移；
//! 只解析 config → 插件 → 逐 db 开库 → 迁移/fixtures。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::DataAccessor;

use crate::args::{FixtureArgs, MigrateArgs, SchemaDiffArgs};
use crate::serve_cmd::{Registries, assemble_plugins, connect_dbs, load_app_config};

/// 瘦身装配产物：目标库句柄（`--db` 选定的 config profile）+ 模块列表（可被 `--module` 过滤）。
struct Slim {
    target: Arc<dyn DataAccessor>,
    modules: Vec<(String, PathBuf)>,
    sql_guard: only_js::bridge::SqlGuard,
}

async fn slim(
    config: Option<&str>,
    site: Option<&str>,
    dir_override: Option<&str>,
    module: Option<&str>,
    db: Option<&str>,
) -> Result<Slim, String> {
    let (cfg, top, config_dir, dir, ts, _base) =
        load_app_config(config, site, dir_override, None, true)?;
    // 迁移/fixtures 作用于 api 目录下的 SQL；强依赖 api 目录（无「纯静态」形态）。
    if !dir.is_dir() {
        return Err(format!(
            "service dir not found: {}（src 源码树或 oj build 产物 dist）",
            dir.display()
        ));
    }
    let mut registries = Registries::default();
    assemble_plugins(&cfg, &top, &config_dir, &mut registries, None).await?;
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
        // 定向匹配两级：① 裸模块名精确匹配（现状保持）；② 未命中时按 dist 版本
        // 目录名 `<module>-<version>` 定向（release 锁聚合形态 `<dir>/<m>-<v>/`；
        // dev 树目录名即裸名，天然落 ①）。不解析 `<m>-<v>` 字符串（version 可含
        // '-'，如 0.1.0-beta，拆分有歧义）——直接对已发现目录名匹配，命中唯一才收。
        if modules.iter().any(|(n, _)| n == m) {
            modules.retain(|(n, _)| n == m);
        } else {
            let hit = modules
                .iter()
                .find(|(_, p)| p.file_name().is_some_and(|f| f.to_string_lossy() == m));
            match hit {
                Some((n, _)) => {
                    let n = n.clone();
                    modules.retain(|(nn, _)| nn == &n);
                }
                None => {
                    let names: Vec<&str> = modules.iter().map(|(n, _)| n.as_str()).collect();
                    return Err(format!(
                        "module {m:?} not found under {}（裸模块名或 dist 版本目录名 \
                         <module>-<version> 皆未命中；已知模块：{names:?}）",
                        dir.display()
                    ));
                }
            }
        }
    }
    Ok(Slim {
        target,
        modules,
        sql_guard: cfg.tenant.sql_guard,
    })
}

/// 错误脱敏（设计稿 §4.1.3）：消息中内嵌的 `<scheme>://user:pass@host/...` 形
/// DSN → 凭据段整体抹为 `***`（`<scheme>://***@host/...`，host/端口保留）。
/// 不限 scheme（mysql/postgres/sqlite 连接错误统一过滤，其他 scheme 带凭据的
/// 也一并抹——对齐根 crate `secret::redact` 的语义并扩展到自由文本）；无
/// `user@host` 权威段的 URL 原样保留；无 DSN 的消息逐字节原样返回。
pub(crate) fn sanitize_dsn(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(pos) = rest.find("://") {
        // 回溯 scheme 词（[A-Za-z0-9+.-]+）——逐 char 回退，UTF-8 边界安全。
        let scheme_start = rest[..pos]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            .map(|(i, _)| i)
            .last()
            .unwrap_or(pos);
        let auth_start = pos + 3;
        if scheme_start == pos {
            // "://" 前无 scheme 词（如 "see ://x"）——原样放行，防误伤。
            out.push_str(&rest[..auth_start]);
            rest = &rest[auth_start..];
            continue;
        }
        // 权威段 = "://" 后到 '/' / '?' / '#' / 空白 / 引号 / 行尾。
        let tail = &rest[auth_start..];
        let auth_len = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '\'' | '"'))
            .unwrap_or(tail.len());
        let authority = &tail[..auth_len];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(&rest[..auth_start]);
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(&rest[..auth_start + auth_len]),
        }
        rest = &tail[auth_len..];
    }
    out.push_str(rest);
    out
}

/// 单模块迁移 + schema.yaml 安全前向收敛（§D1：reconcile 只进 apply 路径）。
/// Err = 该模块失败（json 记入 error 字段 / 人读打 stderr 后继续下一模块——
/// **模块间 continue**；模块内 refinery abort 语义不变：首错即停该模块）。
async fn migrate_one(
    target: &Arc<dyn DataAccessor>,
    name: &str,
    mdir: &Path,
    baseline: bool,
    sql_guard: only_js::bridge::SqlGuard,
    json_mode: bool,
) -> Result<Vec<crate::migrate::AppliedInfo>, String> {
    let applied = crate::migrate::apply_module(target, name, mdir, baseline).await?;
    if !json_mode && !applied.is_empty() {
        println!(
            "oj migrate: {name}: {} applied{}",
            applied.len(),
            if baseline {
                " (baseline, recorded only)"
            } else {
                ""
            }
        );
    }
    if let Some(f) = crate::schema::SchemaFile::load(mdir)? {
        if sql_guard != only_js::bridge::SqlGuard::Off {
            f.validate_tenant(name)?;
        }
        for l in crate::schema::reconcile(target.as_ref(), name, &f).await? {
            if json_mode {
                // json 模式 stdout 只留 JSON 一行——人读行全部改走 stderr。
                eprintln!("oj migrate: {l}");
            } else {
                println!("oj migrate: {l}");
            }
        }
    }
    Ok(applied)
}

/// `--format` 解析：None/`human` → 人读；`json` → json 模式；其余 Err 点名。
fn parse_format(f: Option<&str>) -> Result<bool, String> {
    match f {
        None | Some("human") => Ok(false),
        Some("json") => Ok(true),
        Some(other) => Err(format!(
            "unsupported --format {other:?}（supported: json；缺省人读格式）"
        )),
    }
}

/// 单模块 json 结果：ok=false 时 error 携**脱敏后**原因；applied 含本轮新应用
/// （幂等重跑为空数组；baseline 记账不执行 → 空数组）。
#[derive(serde::Serialize)]
struct JsonResult {
    module: String,
    ok: bool,
    applied: Vec<crate::migrate::AppliedInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `--format json` 输出契约（stdout 恰一行）：模块循环产物在 `results`；
/// 装配期（开库/目录解析）全局失败时 `results` 为空且 `error` 携脱敏原因。
#[derive(serde::Serialize)]
struct JsonOut {
    results: Vec<JsonResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn json_line(out: &JsonOut) -> Result<String, String> {
    serde_json::to_string(out).map_err(|e| format!("json encode: {e}"))
}

/// 收集核心（打印在调用方）：逐模块 continue-and-collect——单模块失败不中断
/// 其余模块。返回 (每模块结果, 新应用迁移总数)。模块内 refinery abort 语义
/// 不变：首错即停该模块。
async fn migrate_collect(
    target: &Arc<dyn DataAccessor>,
    modules: &[(String, PathBuf)],
    baseline: bool,
    sql_guard: only_js::bridge::SqlGuard,
    json_mode: bool,
) -> (Vec<JsonResult>, usize) {
    let mut results: Vec<JsonResult> = Vec::new();
    let mut total = 0;
    for (name, mdir) in modules {
        match migrate_one(target, name, mdir, baseline, sql_guard, json_mode).await {
            Ok(applied) => {
                total += applied.len();
                results.push(JsonResult {
                    module: name.clone(),
                    ok: true,
                    applied,
                    error: None,
                });
            }
            Err(e) => {
                let e = sanitize_dsn(&e);
                eprintln!("oj migrate: {name}: {e}");
                results.push(JsonResult {
                    module: name.clone(),
                    ok: false,
                    applied: Vec::new(),
                    error: Some(e),
                });
            }
        }
    }
    (results, total)
}

/// `oj migrate [-c config] [-d dir] [--db name] [--baseline] [--module M] [--format json]`：
/// 逐模块载入 migrations/ 并 apply 到最新；`--baseline` = 全量记账不执行
/// （P0 建过表的存量库接入门，Q5）；`--db` = 目标库 profile（缺省 default）。
/// `--format json`：continue-and-collect（单模块失败不中断其余模块，聚合进
/// results）+ stdout 一行 JSON + 错误脱敏；退出码任一模块失败即非 0。
pub async fn run_migrate(a: &MigrateArgs) -> Result<(), String> {
    let json_mode = parse_format(a.format.as_deref())?;
    let s = match slim(
        a.config.as_deref(),
        a.site.as_deref(),
        a.dir.as_deref(),
        a.module.as_deref(),
        a.db.as_deref(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            // 全局失败也出 json（安装器可稳定解析 stdout），错误统一脱敏。
            let e = sanitize_dsn(&e);
            if json_mode {
                let out = JsonOut {
                    results: Vec::new(),
                    error: Some(e.clone()),
                };
                println!("{}", json_line(&out)?);
            }
            return Err(e);
        }
    };
    let (results, total) =
        migrate_collect(&s.target, &s.modules, a.baseline, s.sql_guard, json_mode).await;
    let failed: Vec<String> = results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| r.module.clone())
        .collect();
    if json_mode {
        let out = JsonOut {
            results,
            error: None,
        };
        println!("{}", json_line(&out)?);
    } else {
        println!(
            "oj migrate: {total} migration(s) across {} module(s) → db {:?}",
            s.modules.len(),
            a.db.as_deref().unwrap_or("default")
        );
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} of {} module(s) failed: {}",
            failed.len(),
            s.modules.len(),
            failed.join(", ")
        ))
    }
}

/// `oj test fixture [-c config] [-d dir] [--db name] [--module M]`：灌 fixtures/ 演示数据（§4.5）。
pub async fn run_fixture(a: &FixtureArgs) -> Result<(), String> {
    let s = slim(
        a.config.as_deref(),
        a.site.as_deref(),
        a.dir.as_deref(),
        a.module.as_deref(),
        a.db.as_deref(),
    )
    .await?;
    let n = load_fixtures(Some(&s.target), &s.modules).await?;
    println!("oj test fixture: {n} statement(s) loaded");
    Ok(())
}

/// `oj schema diff [-c config] [-d dir] [--db name]`：声明 vs 实库只读对账（D001/D002，§5.1）。
/// 有差异 → 打印报告并 Err（进程退 1，CI 门禁可用）；一致 → in sync。
pub async fn run_schema_diff(a: &SchemaDiffArgs) -> Result<(), String> {
    let s = slim(
        a.config.as_deref(),
        None,
        a.dir.as_deref(),
        None,
        a.db.as_deref(),
    )
    .await?;
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
/// 不记账本——幂等演示数据可重复灌。供 `oj test fixture` 与 `oj test`
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
                // 幂等门禁（执行前 fail-fast，与 S006 seed 同一判据）：注释声称
                // 「幂等可重复灌」，非幂等 INSERT 重灌即撞主键/数据翻倍——不靠 exec 报错兜底。
                if crate::checks::lead_word(stmt) == "INSERT"
                    && !crate::checks::insert_is_idempotent(stmt)
                {
                    return Err(format!(
                        "fixture {name}/{}: 非幂等 INSERT（fixtures 可重复灌，须幂等写法：\
                         INSERT OR IGNORE / ON CONFLICT DO UPDATE / OR REPLACE——同 S006 seed 纪律）",
                        f.display()
                    ));
                }
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
            module: None,
            db: Some("test".into()),
        })
        .await
        .unwrap();
        // 对账同样落在 test profile（迁过的库 in sync；default 未迁 → D001 漂移）。
        run_schema_diff(&SchemaDiffArgs {
            config: Some(cfg(&t)),
            dir: src(&t),
            db: Some("test".into()),
        })
        .await
        .unwrap();
        let e = run_schema_diff(&SchemaDiffArgs {
            config: Some(cfg(&t)),
            dir: src(&t),
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
                module: None,
                db: Some("ghost".into()),
            })
            .await
            .unwrap_err(),
            run_schema_diff(&SchemaDiffArgs {
                config: Some(cfg(&t)),
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
            site: None,
            config: Some(cfg(&t)),
            dir: src(&t),
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
            baseline: false,
            module: None,
            db: None,
            format: Some("xml".into()),
        })
        .await
        .unwrap_err();
        assert!(e.contains("unsupported"), "须点名 unsupported：{e}");
    }
}
