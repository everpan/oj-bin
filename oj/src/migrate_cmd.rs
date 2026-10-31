//! `oj migrate` / `oj test fixture`（§4.6）：**瘦身装配**——不走 `App::from_config`
//! （其证书门禁无逃生口、且携带 seed/路由），CI/运维机无证书也可执行迁移；
//! 只解析 config → 插件 → 逐 db 开库 → 迁移/fixtures。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::DataAccessor;

use crate::args::{FixtureArgs, MigrateArgs, SchemaDiffArgs};
use crate::serve_cmd::{AppConfig, Registries, assemble_plugins, connect_dbs, load_app_config};

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
    module_root: Option<&str>,
) -> Result<Slim, String> {
    let loaded = load_app_config(config, site, dir_override, None, true, module_root)?;
    let AppConfig {
        cfg,
        top,
        config_dir,
        dir,
        ts,
        module_index,
        ..
    } = loaded;
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
    let mut modules = crate::manifest::discover(&dir, ts, module_index.as_deref())?;
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
        a.module_root.as_deref(),
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
        a.module_root.as_deref(),
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
        a.module_root.as_deref(),
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
#[path = "migrate_cmd_tests.rs"]
mod tests;
