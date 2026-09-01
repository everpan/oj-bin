//! 迁移引擎（spec §11.2，D4）：refinery-core 适配器。`OjConn` 把
//! `Arc<dyn DataAccessor>` 包进 refinery 的 `AsyncTransaction`/`AsyncQuery`——
//! 契约只吃 SQL 字符串，跨得过 DataAccessor 边界（§10）；方言决策集中于此。
//! 账本：每模块一张 `_oj_migrations_<module>`（§11.3），version 模块内从 1 起。
//! M001/M002 由 `abort_divergent`/`abort_missing` 承担；并发锁：pg 事务级
//! `pg_advisory_xact_lock`、mysql 靠账本 version 主键冲突兜底（§4.6）。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use refinery_core::traits::r#async::{AsyncMigrate, AsyncQuery, AsyncTransaction};
use refinery_core::{Migration, Report, Target};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use only_js::bridge::{DataAccessor, Dialect};

/// refinery 契约要求的 Error newtype（String 载体）。
#[derive(Debug)]
pub struct MigrateError(String);

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MigrateError {}

fn err(e: impl std::fmt::Display) -> MigrateError {
    MigrateError(e.to_string())
}

/// `;` 朴素切分 + 去空（继承 §2.1 约束：语句内不得含分号字面量）。
fn split_stmts(text: &str) -> Vec<&str> {
    text.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// pg 咨询锁 id：模块名 FNV-1a 64 → i64（跨进程稳定；仅 pg 用）。
fn lock_id_of(module: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in module.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h as i64
}

/// 适配器：refinery 把「整个迁移文件」「账本 INSERT」等作为字符串经 execute
/// 传入；query 走池读账本（execute 内聚事务、外不持连接——sqlite 单连接
/// 池下随后的 get_applied 不自阻塞，§11.2 注①）。
struct OjConn {
    acc: Arc<dyn DataAccessor>,
    module: String,
}

#[async_trait]
impl AsyncTransaction for OjConn {
    type Error = MigrateError;

    async fn execute<'a, T: Iterator<Item = &'a str> + Send>(
        &mut self,
        queries: T,
    ) -> Result<usize, MigrateError> {
        // refinery 把整个迁移文件当一条字符串传入，而 TxSession::exec 是单条
        // 预编译语句 → 先按 ';' 拆分（§11.2 注）。
        let stmts: Vec<&str> = queries.flat_map(split_stmts).collect();
        match self.acc.dialect() {
            // 事务性 DDL：BEGIN → 全部语句（含账本写入）→ COMMIT。
            Dialect::Sqlite | Dialect::Postgres => {
                let tx = self.acc.begin().await.map_err(err)?;
                if self.acc.dialect() == Dialect::Postgres {
                    // 事务级咨询锁：事务结束自动释放（§4.6；refinery 不带锁）。
                    tx.exec(
                        &format!("SELECT pg_advisory_xact_lock({})", lock_id_of(&self.module)),
                        &[],
                    )
                    .await
                    .map_err(err)?;
                }
                for s in &stmts {
                    tx.exec(s, &[]).await.map_err(err)?;
                }
                tx.commit().await.map_err(err)?;
            }
            // mysql DDL 隐式提交：BEGIN 会裂（grouped 必假，§11.2 注③）——
            // 不 BEGIN，顺序执行；互斥靠账本 version 主键冲突兜底。
            Dialect::MySql => {
                for s in &stmts {
                    self.acc.exec_with_params(s, &[]).await.map_err(err)?;
                }
            }
        }
        Ok(stmts.len())
    }
}

#[async_trait]
impl AsyncQuery<Vec<Migration>> for OjConn {
    async fn query(&mut self, query: &str) -> Result<Vec<Migration>, MigrateError> {
        let rows = self.acc.query_with_params(query, &[]).await.map_err(err)?;
        rows.iter()
            .map(|r| {
                let version = r["version"]
                    .as_i64()
                    .ok_or_else(|| err(format!("ledger row: bad version: {r}")))?;
                let name = r["name"]
                    .as_str()
                    .ok_or_else(|| err(format!("ledger row: bad name: {r}")))?
                    .to_string();
                let applied_on = OffsetDateTime::parse(
                    r["applied_on"]
                        .as_str()
                        .ok_or_else(|| err(format!("ledger row: bad applied_on: {r}")))?,
                    &Rfc3339,
                )
                .map_err(err)?;
                let checksum_raw = match r["checksum"].as_str() {
                    Some(s) => s.to_string(),
                    None => r["checksum"].to_string(), // JSON 数字 → 十进制串
                };
                let checksum: u64 = checksum_raw
                    .parse()
                    .map_err(|_| err(format!("ledger row: bad checksum: {r}")))?;
                Ok(Migration::applied(version, name, applied_on, checksum))
            })
            .collect()
    }
}

/// 迁移文件名：`{seq:04}__{desc}[.{dialect}].sql`。返回 (seq, desc, dialect?)。
/// desc 限 `[A-Za-z0-9_]`（refinery `V{seq}__{desc}` 命名约束）。
fn parse_file_name(name: &str) -> Option<(i64, String, Option<String>)> {
    let stem = name.strip_suffix(".sql")?;
    let (stem, dialect) = match stem.rsplit_once('.') {
        Some((s, d)) if matches!(d, "sqlite" | "mysql" | "pg") => (s, Some(d.to_string())),
        _ => (stem, None),
    };
    let (seq, desc) = stem.split_once("__")?;
    let seq = seq.parse().ok()?;
    if !desc.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || desc.is_empty() {
        return None;
    }
    Some((seq, desc.to_string(), dialect))
}

/// S007 载入侧检查 + 方言过滤：`<module>/migrations/` 读入全部 `.sql`，
/// 同 seq 有方言覆盖文件（`0001__init.pg.sql`）时按当前 Dialect 只取其一，
/// 否则回落通用 `0001__init.sql`；seq 必须 1..=n 连续（空洞/乱序/重复报 S007）。
/// BOM 剥离 + CRLF→LF 在此规范化（§11.3，Windows 检出不误判篡改）。
pub fn load_migrations(dir: &Path, dialect: Dialect) -> Result<Vec<Migration>, String> {
    let mut entries: Vec<(i64, String, Option<String>, std::path::PathBuf)> = Vec::new();
    // 无 migrations/ 目录 = 该模块无迁移（合法，非空洞）。
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("S007: read {}: {e}", dir.display())),
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().unwrap().to_string_lossy();
        if !name.ends_with(".sql") {
            continue;
        }
        let Some((seq, desc, dl)) = parse_file_name(&name) else {
            return Err(format!(
                "S007: {} 文件名不合法（须 {{seq:04}}__{{desc}}[.{{sqlite|mysql|pg}}].sql，desc 限 [A-Za-z0-9_]）",
                p.display()
            ));
        };
        entries.push((seq, desc, dl, p));
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    entries.sort_by_key(|(seq, _, _, _)| *seq);
    // S007：同 seq 全部文件（通用 + 各方言覆盖）desc 必须一致——账本 name 的
    // 唯一来源（M001 对比依据，§4.7）；两个通用文件同 seq 必然 desc 相异 → 一并覆盖。
    let mut by_seq: std::collections::BTreeMap<i64, (String, std::path::PathBuf)> =
        std::collections::BTreeMap::new();
    for (seq, desc, _, p) in &entries {
        match by_seq.get(seq) {
            Some((prev, _)) if prev != desc => {
                return Err(format!(
                    "S007: 序号 {seq} 两文件 desc 不一致：{prev:?} 与 {desc:?}（{}）\n  \
                     下一步：方言覆盖文件须同名 desc（如 0001__init.sql + 0001__init.pg.sql）",
                    p.display()
                ));
            }
            _ => {
                by_seq.insert(*seq, (desc.clone(), p.clone()));
            }
        }
    }
    // seq 连续性（空洞/乱序）——按去重后的 seq 数（方言覆盖文件不重复计数）。
    for (i, (seq, (_, p))) in by_seq.iter().enumerate() {
        let expected = i as i64 + 1;
        if *seq != expected {
            return Err(format!(
                "S007: 序号空洞/乱序：第 {expected} 个迁移的 seq 是 {seq:04}（{}）\n  下一步：重排 migrations/ 使 seq 从 1 连续递增",
                p.display()
            ));
        }
    }
    // 方言选择（确定性）：方言覆盖文件必胜，通用文件仅兜底；其他方言文件跳过。
    let mut chosen: std::collections::HashMap<i64, (String, std::path::PathBuf)> =
        std::collections::HashMap::new();
    let cur = dialect_tag(dialect);
    for (seq, desc, dl, p) in &entries {
        match dl.as_deref() {
            Some(d) if d == cur => {
                chosen.insert(*seq, (desc.clone(), p.clone()));
            }
            Some(_) => {}
            None => {
                chosen
                    .entry(*seq)
                    .or_insert_with(|| (desc.clone(), p.clone()));
            }
        }
    }
    let mut out = Vec::new();
    for (seq, (desc, p)) in chosen {
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
        let sql = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
        let m = Migration::unapplied(&format!("V{seq}__{desc}"), &sql)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        out.push(m);
    }
    out.sort_by_key(|m| m.version());
    Ok(out)
}

fn dialect_tag(d: Dialect) -> &'static str {
    match d {
        Dialect::Sqlite => "sqlite",
        Dialect::MySql => "mysql",
        Dialect::Postgres => "pg",
    }
}

/// 单模块迁移：账本 `_oj_migrations_<module>`，abort_divergent/abort_missing=true、
/// grouped=false（mysql DDL 隐式提交下 grouped 必裂）、Target 由调用方给
/// （Latest=正常迁移；Fake=`--baseline` 全量记账不执行，Q5）。
pub async fn run_module(
    acc: Arc<dyn DataAccessor>,
    module: &str,
    migrations: &[Migration],
    target: Target,
) -> Result<Report, String> {
    let mut conn = OjConn {
        acc,
        module: module.to_string(),
    };
    conn.migrate(
        migrations,
        /*abort_divergent=*/ true,
        /*abort_missing=*/ true,
        /*grouped=*/ false,
        target,
        &ledger_name(module),
    )
    .await
    .map_err(|e| e.to_string())
}

/// `AsyncMigrate` 全是 provided methods（§10：实现 2 个只吃 SQL 字符串的 trait 即得
/// 整个迁移引擎）——空 impl 启用 `migrate()`/`get_applied_migrations()` 等。
impl AsyncMigrate for OjConn {}

/// 账本表名（每模块一张，§11.3）。pub 供 verify/运维指引复用。
pub fn ledger_name(module: &str) -> String {
    format!("_oj_migrations_{module}")
}

/// 账本 DDL：与 refinery-core 0.9 `assert_migrations_table_query`（int8-versions
/// feature）逐字同构——该函数 pub(crate) 不可复用，此处镜像；三方言均支持
/// `CREATE TABLE IF NOT EXISTS`（refinery 模板用 `CREATE TABLE`，需先建版才用得上 IF NOT EXISTS）。
const LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS {t} (\
         version int8 PRIMARY KEY, \
         name VARCHAR(255), \
         applied_on VARCHAR(255), \
         checksum VARCHAR(255));";

/// 单模块 apply（无迁移目录 → 0）。baseline=true → Target::Fake（记账不执行）。
pub async fn apply_module(
    acc: &Arc<dyn DataAccessor>,
    module: &str,
    mdir: &Path,
    baseline: bool,
) -> Result<usize, String> {
    let migs = load_migrations(&mdir.join("migrations"), acc.dialect())?;
    if migs.is_empty() {
        return Ok(0);
    }
    let target = if baseline {
        Target::Fake
    } else {
        Target::Latest
    };
    Ok(run_module(acc.clone(), module, &migs, target)
        .await?
        .applied_migrations()
        .len())
}

/// 单模块 verify（release 启动校验，§4.6）：
/// M003 账本 seq 超过产物最大 seq（降版部署）→ 拒启；
/// M004 存在待应用（含首启空账本）→ 拒启并给出命令。
pub async fn verify_module(
    acc: &Arc<dyn DataAccessor>,
    module: &str,
    mdir: &Path,
) -> Result<(), String> {
    let migs = load_migrations(&mdir.join("migrations"), acc.dialect())?;
    if migs.is_empty() {
        return Ok(());
    }
    let mut conn = OjConn {
        acc: acc.clone(),
        module: module.to_string(),
    };
    // get_applied_migrations 不建账本表（refinery 只在 migrate() 里 assert）——
    // 首启空库须先建，否则裸 "no such table" 错误外漏（§4.6 首启拒启语义靠 M004）。
    conn.execute([LEDGER_DDL.replace("{t}", &ledger_name(module)).as_str()].into_iter())
        .await
        .map_err(|e| format!("module {module}: assert ledger: {e}"))?;
    let applied = conn
        .get_applied_migrations(&ledger_name(module))
        .await
        .map_err(|e| e.to_string())?;
    let head = migs.last().unwrap().version();
    let ahead: Vec<String> = applied
        .iter()
        .filter(|m| m.version() > head)
        .map(|m| m.version().to_string())
        .collect();
    if !ahead.is_empty() {
        return Err(format!(
            "M003: 模块 {module} 账本 seq（{}）超过产物 migrations 最大 seq（{head}）\
             ——降版部署被禁止（§4.6）\n  下一步：部署含最新迁移的产物；\
             人工回退账本须同步回退 DDL 并自担数据风险",
            ahead.join(",")
        ));
    }
    let applied_v: std::collections::HashSet<i64> = applied.iter().map(|m| m.version()).collect();
    let pending: Vec<&str> = migs
        .iter()
        .filter(|m| !applied_v.contains(&m.version()))
        .map(|m| m.name())
        .collect();
    if !pending.is_empty() {
        return Err(format!(
            "M004: 模块 {module} 有 {} 个待应用迁移（{}）——verify 模式拒启\n  \
             下一步：oj migrate -c <config> -d <dir>",
            pending.len(),
            pending.join(", ")
        ));
    }
    Ok(())
}

/// 全模块（按名排序）apply。default 库缺失 → warn 跳过（同 seed 语义）。
pub async fn apply_all(
    default: Option<&Arc<dyn DataAccessor>>,
    dir: &Path,
    ts: bool,
    baseline: bool,
) -> Result<(), String> {
    let Some(acc) = default else {
        eprintln!("warn: migrate skipped (no default db)");
        return Ok(());
    };
    for (name, mdir) in crate::manifest::discover(dir, ts)? {
        let n = apply_module(acc, &name, &mdir, baseline)
            .await
            .map_err(|e| format!("module {name}: {e}"))?;
        if n > 0 {
            eprintln!(
                "migrate: {name}: {n} applied{}",
                if baseline {
                    " (baseline, recorded only)"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

/// 全模块（按名排序）verify。
pub async fn verify_all(
    default: Option<&Arc<dyn DataAccessor>>,
    dir: &Path,
    ts: bool,
) -> Result<(), String> {
    let Some(acc) = default else {
        return Ok(());
    };
    for (name, mdir) in crate::manifest::discover(dir, ts)? {
        verify_module(acc, &name, &mdir)
            .await
            .map_err(|e| format!("module {name}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::DbBackendRegistry;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "oj-mig-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &std::path::Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    async fn sqlite() -> Arc<dyn DataAccessor> {
        DbBackendRegistry::builtin()
            .connect("sqlite::memory:", Path::new("."))
            .await
            .unwrap()
    }

    async fn tables(acc: &Arc<dyn DataAccessor>) -> Vec<String> {
        acc.query("select name from sqlite_master where type='table' order by name")
            .await
            .unwrap()
            .iter()
            .filter_map(|r| r["name"].as_str().map(str::to_string))
            .collect()
    }

    async fn ledger(acc: &Arc<dyn DataAccessor>, module: &str) -> Vec<(i64, String)> {
        acc.query(&format!(
            "select version, name from _oj_migrations_{module} order by version"
        ))
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["version"].as_i64().unwrap(),
                r["name"].as_str().unwrap().into(),
            )
        })
        .collect()
    }

    /// spike 主验证：refinery 全链路（建账本 → apply → 幂等重跑 → 账本行齐）。
    #[tokio::test(flavor = "current_thread")]
    async fn fresh_apply_then_idempotent_rerun() {
        let d = tmpdir("main");
        let mig = d.join("migrations");
        write(&mig.join("0001__init.sql"), "CREATE TABLE t1 (x);");
        write(&mig.join("0002__more.sql"), "CREATE TABLE t2 (x);");
        let migs = load_migrations(&mig, Dialect::Sqlite).unwrap();
        assert_eq!(migs.len(), 2);
        let acc = sqlite().await;
        let r1 = run_module(acc.clone(), "m", &migs, Target::Latest)
            .await
            .unwrap();
        assert_eq!(r1.applied_migrations().len(), 2);
        assert!(tables(&acc).await.contains(&"t1".into()));
        // 账本 name 存 refinery 解析后的 desc（V1__init → "init"）。
        assert_eq!(
            ledger(&acc, "m").await,
            vec![(1, "init".into()), (2, "more".into())]
        );
        // 幂等：重跑零新增。
        let r2 = run_module(acc.clone(), "m", &migs, Target::Latest)
            .await
            .unwrap();
        assert_eq!(r2.applied_migrations().len(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// M001：账本存量 checksum 与文件重算不一致 → divergent 拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn tampered_sql_is_divergent() {
        let d = tmpdir("div");
        let mig = d.join("migrations");
        write(&mig.join("0001__init.sql"), "CREATE TABLE t1 (x);");
        let acc = sqlite().await;
        let migs = load_migrations(&mig, Dialect::Sqlite).unwrap();
        run_module(acc.clone(), "m", &migs, Target::Latest)
            .await
            .unwrap();
        // 篡改文件内容（同版本不同 SQL）。
        write(&mig.join("0001__init.sql"), "CREATE TABLE t1 (y);");
        let migs2 = load_migrations(&mig, Dialect::Sqlite).unwrap();
        let e = run_module(acc.clone(), "m", &migs2, Target::Latest)
            .await
            .unwrap_err();
        assert!(
            e.contains("Divergent") || e.contains("different than"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// M002：账本有 V2 但文件系统只剩 V1 → missing 拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn missing_file_is_aborted() {
        let d = tmpdir("miss");
        let mig = d.join("migrations");
        write(&mig.join("0001__init.sql"), "CREATE TABLE t1 (x);");
        write(&mig.join("0002__gone.sql"), "CREATE TABLE t2 (x);");
        let acc = sqlite().await;
        run_module(
            acc.clone(),
            "m",
            &load_migrations(&mig, Dialect::Sqlite).unwrap(),
            Target::Latest,
        )
        .await
        .unwrap();
        std::fs::remove_file(mig.join("0002__gone.sql")).unwrap();
        let e = run_module(
            acc.clone(),
            "m",
            &load_migrations(&mig, Dialect::Sqlite).unwrap(),
            Target::Latest,
        )
        .await
        .unwrap_err();
        assert!(e.contains("missing") || e.contains("Missing"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Q5 baseline：Target::Fake 全量记账不执行（存量库 P0 已建表的接入门）。
    #[tokio::test(flavor = "current_thread")]
    async fn baseline_fake_records_without_executing() {
        let d = tmpdir("base");
        let mig = d.join("migrations");
        write(&mig.join("0001__init.sql"), "CREATE TABLE t1 (x);");
        let acc = sqlite().await;
        run_module(
            acc.clone(),
            "m",
            &load_migrations(&mig, Dialect::Sqlite).unwrap(),
            Target::Fake,
        )
        .await
        .unwrap();
        // 账本已记、表未建——之后补 apply 不再执行（存量库语义）。
        assert_eq!(ledger(&acc, "m").await.len(), 1);
        assert!(!tables(&acc).await.contains(&"t1".into()));
        let r = run_module(
            acc.clone(),
            "m",
            &load_migrations(&mig, Dialect::Sqlite).unwrap(),
            Target::Latest,
        )
        .await
        .unwrap();
        assert_eq!(r.applied_migrations().len(), 0);
        assert!(!tables(&acc).await.contains(&"t1".into()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 事务性：中途失败整体回滚（迁移 sql 与账本写入同事务）。
    #[tokio::test(flavor = "current_thread")]
    async fn failing_migration_rolls_back_ledger() {
        let d = tmpdir("rb");
        let mig = d.join("migrations");
        write(&mig.join("0001__ok.sql"), "CREATE TABLE t1 (x);");
        write(
            &mig.join("0002__bad.sql"),
            "CREATE TABLE t1 (x); CREATE TABLE nope.broken (x);",
        );
        let acc = sqlite().await;
        let e = run_module(
            acc.clone(),
            "m",
            &load_migrations(&mig, Dialect::Sqlite).unwrap(),
            Target::Latest,
        )
        .await
        .unwrap_err();
        assert!(
            e.contains("0002") || e.contains("V2") || e.contains("broken"),
            "{e}"
        );
        // refinery 每迁移一事务：V1 已提交（t1 + 账本行在）；V2 随自身事务回滚
        // （sqlite 事务性 DDL），账本无 V2 残留。
        assert!(tables(&acc).await.contains(&"t1".into()));
        assert!(!tables(&acc).await.contains(&"t2".into()));
        assert_eq!(ledger(&acc, "m").await, vec![(1, "ok".into())]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// S007：空洞 / 重复 / 乱序命名 / 方言对 desc 不一致 → Err。
    #[test]
    fn s007_gaps_duplicates_and_dialect_mismatch() {
        let d = tmpdir("s007");
        // 每个场景用各自的目录，而不是复用 `migrations` 后删除重建：Windows 的删除
        // 是延迟的（delete-on-close），残留句柄（索引器/杀软）会让紧随其后的重建
        // 失败。不删就不需要等。
        // 空洞：缺 0002。
        let mig = d.join("gap/migrations");
        write(&mig.join("0001__a.sql"), "x;");
        write(&mig.join("0003__c.sql"), "x;");
        let e = load_migrations(&mig, Dialect::Sqlite).unwrap_err();
        assert!(e.contains("S007") && e.contains("0003"), "{e}");
        // 通用文件同 seq 重复。
        let mig = d.join("dup/migrations");
        write(&mig.join("0001__a.sql"), "x;");
        write(&mig.join("0001__b.sql"), "x;");
        let e = load_migrations(&mig, Dialect::Sqlite).unwrap_err();
        assert!(e.contains("不一致"), "{e}");
        // 方言对 desc 不一致。
        let mig = d.join("dialect-pair/migrations");
        write(&mig.join("0001__a.sql"), "x;");
        write(&mig.join("0001__b.pg.sql"), "x;");
        let e = load_migrations(&mig, Dialect::Sqlite).unwrap_err();
        assert!(e.contains("S007"), "{e}");
        // 非法文件名。
        let mig = d.join("bad-name/migrations");
        write(&mig.join("init.sql"), "x;");
        let e = load_migrations(&mig, Dialect::Sqlite).unwrap_err();
        assert!(e.contains("文件名不合法"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 方言覆盖：pg 取 `0001__init.pg.sql`、sqlite 取通用文件；账本 name 一致
    /// （(seq,desc) 映射相同，§4.7）；BOM/CRLF 规范化生效。
    #[test]
    fn dialect_override_selection_and_normalization() {
        let d = tmpdir("dialect");
        let mig = d.join("migrations");
        write(&mig.join("0001__init.sql"), "CREATE TABLE t1 (x);");
        write(
            &mig.join("0001__init.pg.sql"),
            "CREATE TABLE t1 (x) PARTITION BY nada;",
        );
        write(
            &mig.join("0002__bom.sql"),
            "\u{feff}CREATE TABLE t2 (x);\r\n",
        );
        let pg = load_migrations(&mig, Dialect::Postgres).unwrap();
        assert_eq!(pg.len(), 2);
        assert!(
            pg[0].sql().unwrap().contains("PARTITION"),
            "{:?}",
            pg[0].sql()
        );
        let lite = load_migrations(&mig, Dialect::Sqlite).unwrap();
        assert!(
            lite[0].sql().unwrap().contains("(x);")
                && !lite[0].sql().unwrap().contains("PARTITION")
        );
        // 账本 name 一致（方言段剥掉）。
        assert_eq!(pg[0].name(), lite[0].name());
        // BOM/CRLF 已规范化。
        assert!(
            !lite[1].sql().unwrap().contains('\r')
                && !lite[1].sql().unwrap().starts_with('\u{feff}')
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
