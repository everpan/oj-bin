//! 模块级种子重放（spec P0）：`<module>/seed.<dialect>.sql` 优先、`<module>/seed.sql`
//! 兜底，三方言 default 库随启动重放（与迁移的 `{seq}__{desc}.{dialect}.sql` 同一套
//! 方言覆盖语义：方言件胜出、通用件兜底，**绝不双跑**）。
//! 幂等 SQL、按 `;` 朴素切分（语句内不得含分号字面量，§2.1）；执行顺序 = 模块目录名
//! 排序。幂等写法以 sqlite 惯用法为源（`INSERT OR IGNORE`），引擎按目标方言自动改写
//! 关键字（mysql → `INSERT IGNORE`、pg → 句尾 `ON CONFLICT DO NOTHING`）；方言件写
//! 原生语法（`INSERT IGNORE` / `ON CONFLICT DO NOTHING`）则原样透传。
//! 每条语句的执行与结果（受影响行数）记 tracing 日志（server 落 logs/，CLI 落 stderr）。
//! S002：同一张表被两处 `CREATE TABLE` → 启动 fail-fast，不静默合并（§8-1）。
//! fixtures/ 不重放（演示数据，由 `oj test fixture` 灌入）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::{DataAccessor, Dialect, ModuleIndex};

/// 通用种子文件名（方言件的兜底）。
pub(crate) const SEED_GENERIC: &str = "seed.sql";
/// 方言件前缀（`seed.<tag>.sql`）。与 `SEED_GENERIC` 同源，避免多处字面量漂移。
const SEED_PREFIX: &str = "seed.";
const SEED_EXT: &str = ".sql";

/// 已知方言文件名（顺序即 [`scan`] 的候选返回序）。
const DIALECTS: [Dialect; 3] = [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres];

/// 当前方言的种子文件名：`seed.<tag>.sql`，标签与迁移的 `{desc}.{dialect}.sql` 同一套
/// （`migrate::dialect_tag`：sqlite / mysql / pg）。
fn seed_name(d: Dialect) -> String {
    format!("{SEED_PREFIX}{}{SEED_EXT}", crate::migrate::dialect_tag(d))
}

/// 一次目录扫描的产物：规范候选 + 命名不规范的 seed 文件。
pub struct SeedSet {
    /// 实际存在的规范候选，顺序固定：通用件 → sqlite → mysql → pg。
    pub candidates: Vec<PathBuf>,
    /// (标签, 路径)：标签未知（`seed.postgres.sql`）或大小写不符（`SEED.MYSQL.SQL`）。
    pub misnamed: Vec<(String, PathBuf)>,
}

/// 扫一个模块目录，按**文件名精确匹配**取候选（不靠 `is_file` 的大小写不敏感行为：
/// macOS / Windows 上 `SEED.MYSQL.SQL` 会被 `is_file("seed.mysql.sql")` 命中并执行，
/// Linux 上不会——规范化判定必须跨平台一致）。
/// 运行时选择（[`collect`]）与静态检查（S003/S006）共用这一份结果。
pub fn scan(root: &Path) -> SeedSet {
    let mut set = SeedSet {
        candidates: Vec::new(),
        misnamed: Vec::new(),
    };
    let Ok(rd) = std::fs::read_dir(root) else {
        return set;
    };
    let mut files: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical: Vec<String> = std::iter::once(SEED_GENERIC.to_string())
        .chain(DIALECTS.iter().map(|d| seed_name(*d)))
        .collect();
    for want in &canonical {
        if let Some((_, p)) = files.iter().find(|(n, _)| n == want) {
            set.candidates.push(p.clone());
        }
    }
    for (name, path) in files {
        if set.candidates.iter().any(|p| p == &path) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        // 通用件没有标签，单独判（`strip_prefix("seed.")` 得到的是 "sql"，不是空串）。
        let stem = if lower == SEED_GENERIC {
            String::new()
        } else {
            let Some(s) = lower
                .strip_prefix(SEED_PREFIX)
                .and_then(|s| s.strip_suffix(SEED_EXT))
            else {
                continue;
            };
            s.to_string()
        };
        // `seed.sql` 的大小写变体（空 stem）没有标签可说：tag 置空，调用方提示「应为全小写」。
        // 该分支同时保证下面的字节切片区间非空，不会在短名字上反转 panic。
        let tag = if stem.is_empty() {
            String::new()
        } else {
            name[SEED_PREFIX.len()..name.len() - SEED_EXT.len()].to_string()
        };
        set.misnamed.push((tag, path));
    }
    set
}

/// 一个模块目录下**实际存在**的规范 seed 候选（通用件 + 三个方言件）。
pub fn seed_candidates(root: &Path) -> Vec<PathBuf> {
    scan(root).candidates
}

/// 从文件名推断该 seed 件的方言：`seed.sql` → `None`（通用件，三方言都必须能跑）。
pub fn seed_dialect_of(p: &Path) -> Option<Dialect> {
    let name = p.file_name()?.to_string_lossy();
    let stem = name
        .strip_prefix(SEED_PREFIX)
        .and_then(|s| s.strip_suffix(SEED_EXT))?;
    DIALECTS
        .into_iter()
        .find(|d| crate::migrate::dialect_tag(*d) == stem)
}

/// 按方言选文件：方言件存在即胜出，通用件只兜底（绝不双跑）。
struct Pick {
    /// 实际执行的文件。
    chosen: Option<PathBuf>,
    /// 已知方言件里与当前方言不匹配的那些（仅在无可用文件时非空）。
    skipped: Vec<PathBuf>,
    /// 方言标签写错的 seed 文件（任何情况下都报告）。
    misnamed: Vec<(String, PathBuf)>,
}

fn pick(root: &Path, dialect: Dialect) -> Pick {
    let set = scan(root);
    let specific = root.join(seed_name(dialect));
    let generic = root.join(SEED_GENERIC);
    let mut chosen = None;
    let mut skipped = Vec::new();
    if set.candidates.contains(&specific) {
        chosen = Some(specific);
    } else if set.candidates.contains(&generic) {
        chosen = Some(generic);
    } else {
        skipped = set.candidates;
    }
    Pick {
        chosen,
        skipped,
        misnamed: set.misnamed,
    }
}

/// seed 收集结果。
pub struct Collected {
    /// (模块名, 实际要执行的文件)，已按模块名排序。
    pub files: Vec<(String, PathBuf)>,
    /// (模块名, 因方言不匹配而跳过的文件)：模块只有别的方言件且无通用件 → 起服要 warn。
    pub skipped: Vec<(String, PathBuf)>,
    /// (模块名, 标签, 文件)：文件名标签拼错 → 起服要 warn、`oj check` 要报错。
    pub misnamed: Vec<(String, String, PathBuf)>,
}

/// 模块根清单：`index` 覆盖 `dir` 时按索引（模块根可不在首层），否则首层子目录。
/// 复用 `manifest::discover` 的集合判定，不另扫一套。
fn module_roots(dir: &Path, index: Option<&ModuleIndex>) -> Vec<(String, PathBuf)> {
    if let Some(idx) = index {
        return idx
            .modules
            .iter()
            .map(|(name, e)| (name.clone(), e.root.clone()))
            .collect();
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|d| (d.file_name().unwrap().to_string_lossy().into_owned(), d))
        .collect()
}

/// 是否存在**任何** seed 候选（用于「无 default 库」的告警判定：零 seed 的工程
/// 不该因为没配库而每次启动都刷一条 warn）。
fn any_seed(dir: &Path, index: Option<&ModuleIndex>) -> bool {
    module_roots(dir, index).iter().any(|(_, root)| {
        let set = scan(root);
        !set.candidates.is_empty() || !set.misnamed.is_empty()
    })
}

/// 收集各模块的 seed（dev: `src/<m>/`，release: `dist/<m>-<v>/`），按模块名排序。
///
/// 模块根由 [`module_roots`] 定位（`index` 覆盖时模块根可不在首层，首层扫会**静默漏掉
/// 种子**——评审（丁）：种子不灌且无报错比启动失败更坏）。
pub fn collect(dir: &Path, index: Option<&ModuleIndex>, dialect: Dialect) -> Collected {
    let mut out = Collected {
        files: Vec::new(),
        skipped: Vec::new(),
        misnamed: Vec::new(),
    };
    for (name, root) in module_roots(dir, index) {
        let p = pick(&root, dialect);
        if let Some(f) = p.chosen {
            out.files.push((name.clone(), f));
        }
        for f in p.skipped {
            out.skipped.push((name.clone(), f));
        }
        for (tag, f) in p.misnamed {
            out.misnamed.push((name.clone(), tag, f));
        }
    }
    out
}

/// 提取 SQL 文本里 `CREATE TABLE` 的表名（去 `--` 行注释后扫描；
/// `CREATE INDEX` 与 `INSERT INTO` 不算建表）。认 `IF NOT EXISTS`、引号
/// （"t" / \`t\` / [t]）与 schema 限定（main.t → t）。重复出现不去重（调用方只比对首见）。
pub fn create_tables(sql: &str) -> Vec<String> {
    // 去注释：整行以 -- 开头的行剔除（语句中段的行内注释不常见，不做）。
    let code = sql
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find_ci(&code, "create table", i) {
        let rest = code[p + "create table".len()..].trim_start();
        let rest = if find_ci(rest, "if not exists", 0) == Some(0) {
            rest["if not exists".len()..].trim_start()
        } else {
            rest
        };
        if let Some(name) = take_ident(rest) {
            out.push(name);
        }
        i = p + "create table".len();
    }
    out
}

/// 大小写不敏感子串查找（ASCII 关键字；UTF-8 续字节 ≥ 0x80 不会误命中 ASCII）。
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// 从位置 0 取标识符：引号包裹（"t" / \`t\` / [t]）或裸词；带 schema 限定
/// （`main.t` / `"main".t`）取末段。非标识符开头 → None。
fn take_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let (raw, rest) = match s.chars().next()? {
        '"' => quoted(s, '"')?,
        '`' => quoted(s, '`')?,
        '[' => {
            let e = s.find(']')?;
            (&s[1..e], &s[e + 1..])
        }
        _ => {
            let e = s
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(s.len());
            if e == 0 {
                return None;
            }
            (&s[..e], &s[e..])
        }
    };
    let rest = rest.trim_start();
    if let Some(dot_rest) = rest.strip_prefix('.') {
        return Some(take_ident(dot_rest).unwrap_or_else(|| raw.to_string()));
    }
    Some(raw.to_string())
}

/// 取 `q` 包裹的片段，返回 (内容, 结束后的剩余串)。
fn quoted(s: &str, q: char) -> Option<(&str, &str)> {
    let e = s[1..].find(q)? + 1;
    Some((&s[1..e], &s[e + 1..]))
}

/// 诊断文案里的路径：统一以 `/` 分隔。Windows 原生分隔符 `\` 会让同一条报错在
/// 跨平台下不可比对（测试按 `模块/seed.sql` 断言）。
fn show(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `;` 朴素切分 + 去空（继承 §2.1 约束：语句内不得含分号字面量）。
fn split_statements(text: &str) -> Vec<&str> {
    text.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// 剥前置 `--` 行注释与空白：`;` 朴素切分后 chunk 头部可能是注释行，直接按
/// 「INSERT 起头」判定会漏改写（v0.1.65 实锤：seed 语句前置注释行让
/// `INSERT OR IGNORE` 原样下发 mysql → 1064）。整段皆注释 → 空串。
fn strip_leading_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        match s.strip_prefix("--") {
            None => return s,
            Some(rest) => match rest.find('\n') {
                Some(i) => s = &rest[i + 1..],
                None => return "",
            },
        }
    }
}

/// seed 幂等写法以 sqlite 惯用法为源：`INSERT OR IGNORE INTO …`。按目标方言改写
/// 关键字：mysql → `INSERT IGNORE`；pg → 剥 `OR IGNORE`、句尾追加 `ON CONFLICT DO
/// NOTHING`。其余形态（`OR REPLACE` / `ON CONFLICT` / `ON DUPLICATE KEY`）与非
/// INSERT 语句一律原样透传——作者显式选择的方言写法不猜测改写。
/// 判定前先剥前置注释（见 [`strip_leading_comments`]）；未命中改写时保留原文。
pub fn translate_insert<'a>(stmt: &'a str, d: Dialect) -> Cow<'a, str> {
    if d == Dialect::Sqlite {
        return Cow::Borrowed(stmt);
    }
    let body = strip_leading_comments(stmt);
    // 仅当语句以 INSERT 起头、紧跟空白 + OR IGNORE + 词边界时改写。
    if !body
        .get(..6)
        .is_some_and(|h| h.eq_ignore_ascii_case("INSERT"))
        || !body[6..].starts_with(|c: char| c.is_ascii_whitespace())
    {
        return Cow::Borrowed(stmt);
    }
    let after = body[6..].trim_start();
    if !after
        .get(..9)
        .is_some_and(|h| h.eq_ignore_ascii_case("or ignore"))
        || !after[9..].starts_with(|c: char| c.is_ascii_whitespace())
    {
        return Cow::Borrowed(stmt);
    }
    let rest = after[9..].trim_start();
    match d {
        Dialect::MySql => Cow::Owned(format!("INSERT IGNORE {rest}")),
        _ => Cow::Owned(format!("INSERT {rest} ON CONFLICT DO NOTHING")),
    }
}

/// 启动重放（from_config 调用）：各模块按当前方言取 `seed.<tag>.sql` / `seed.sql`
/// （目录名排序，方言件胜出、通用件兜底）。
/// 无种子文件 → 静默返回；有种子但 default 库缺失 → warn 跳过；模块只有别的方言件
/// 且无通用件 → warn 点名（该模块本轮无种子）。
/// 三方言都重放（幂等靠 S006 门禁 + 方言改写 + SQL 自身）；先全量冲突检查（S002）
/// 再执行——失败不落任何副作用。
pub async fn replay_all(
    default: Option<&Arc<dyn DataAccessor>>,
    dir: &Path,
    index: Option<&ModuleIndex>,
) -> Result<(), String> {
    let Some(db) = default else {
        // 零 seed 的工程（静态站 / 纯 kv API）不该因为没配库而每次启动刷 warn。
        if any_seed(dir, index) {
            tracing::warn!("seed skipped: no default db");
            eprintln!("warn: seed skipped (no default db)");
        }
        return Ok(());
    };
    // 门控（T8）：未知方言（不在 builder-known 子集）一律 fail-loud，错误文本点名方言，
    // 绝不回落 Sqlite。复用 db_error::unsupported 构造器渲染文本。
    // 顺序：门控**先于**收集——零 seed 的工程同样要拦（否则方言选文件无从谈起）。
    let dialect_name = db.dialect_info().name.clone();
    let Some(dialect) = Dialect::known(&dialect_name) else {
        return Err(only_js::bridge::db_error::unsupported(&dialect_name, "seed").message);
    };
    let plan = collect(dir, index, dialect);
    // 缺口告警：模块只有**别的**方言的 seed 件、且没有通用件 → 该模块本轮没有种子。
    // 「模块没有 seed」是常态（业务模块大多不灌），不能全量 warn，只在这一种情形提示。
    if !plan.skipped.is_empty() {
        let missing = seed_name(dialect);
        // 按模块分组：同一模块可能同时放着两份别的方言件，一条 warn 报全。
        let mut by_module: std::collections::BTreeMap<&str, Vec<String>> =
            std::collections::BTreeMap::new();
        for (name, p) in &plan.skipped {
            by_module.entry(name.as_str()).or_default().push(show(p));
        }
        for (name, files) in by_module {
            let list = files.join(", ");
            tracing::warn!(
                module = name,
                files = %list,
                dialect = dialect.as_str(),
                "seed skipped: no matching dialect file"
            );
            eprintln!(
                "warn: seed skipped: {name} 只有 {list}，当前方言 {} 需要 {missing} 或 {SEED_GENERIC}",
                dialect.as_str()
            );
        }
    }
    // 文件名标签拼错（如 seed.postgres.sql）→ 永远不会被执行，必须点名。
    for (name, tag, p) in &plan.misnamed {
        let why = if tag.is_empty() {
            "文件名大小写不符（只认全小写 seed.sql）".to_string()
        } else {
            let hint = if tag.eq_ignore_ascii_case("postgres") {
                "（postgres 要写成 pg）"
            } else {
                ""
            };
            format!("方言标签 {tag:?} 未知{hint}——只认 sqlite / mysql / pg（全小写）")
        };
        tracing::warn!(
            module = name,
            file = %show(p),
            tag = tag.as_str(),
            "seed skipped: unknown dialect tag in file name"
        );
        eprintln!(
            "warn: seed skipped: {name} 的 {} {why}，本轮不会执行",
            show(p)
        );
    }
    if plan.files.is_empty() {
        return Ok(());
    }
    let mut texts = Vec::new();
    for (name, p) in &plan.files {
        let t = std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", show(p)))?;
        texts.push((name.as_str(), p.clone(), t));
    }
    // S002 冲突检查：表 → 首见文件；重复即 fail-fast（不执行任何语句）。
    let mut owner: HashMap<&str, &Path> = HashMap::new();
    for (_, p, t) in &texts {
        for name in create_tables(t) {
            if let Some(prev) = owner.get(name.as_str()) {
                return Err(format!(
                    "S002: 表 `{name}` 被多处建表：{} 与 {}\n  \
                     原因：模块自治要求 表→模块 单射（§8-1），不静默合并。\n  \
                     下一步：保留唯一归属处的建表与数据，删除另一处后重试。",
                    show(prev),
                    show(p)
                ));
            }
            owner.insert(leak_str(name), p.as_path());
        }
    }
    // 逐语句只在失败时响（error）；成功按文件合并为一条统计（含语句数）。
    for (name, p, t) in &texts {
        let mut applied = 0usize;
        for (i, stmt) in split_statements(t).into_iter().enumerate() {
            let sql = translate_insert(stmt, dialect);
            if let Err(e) = db.exec_with_params(&sql, &[]).await {
                tracing::error!(
                    module = name,
                    file = %show(p),
                    seq = i,
                    stmt = %crate::migrate::log_snip(stmt),
                    "seed failed: {e}"
                );
                return Err(format!("seed {} (seq {i}): {e}", show(p)));
            }
            applied += 1;
        }
        tracing::info!(
            module = name,
            file = %show(p),
            dialect = dialect.as_str(),
            stmts = applied,
            "seed ok"
        );
    }
    Ok(())
}

/// 借用表名进 owner 的键（进程级启动期一次性，泄漏量 = 建表数，可忽略）。
fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

// ===== `oj migrate --with-seed`（v0.1.125，spec 2026-11-07）====================
//
// 与冷启动 `replay_all` 的关系：**只共享执行核心**，不复用其 `collect` / 门控 /
// skipped+misnamed 告警 / `any_seed` 早退（那些逐字留给冷启动，保持 12 个单测基线）。
// 新入口吃 `run_migrate` 现成的 `s.modules`（已过 `--module` 过滤）与 `s.target`，
// 并把信任边界不同的运行期 fail-closed 门禁装在这条 opt-in 路径上（§6）。

/// `--with-seed` 逐模块结果（JSON 顶层 `seed.results[]` 元素）。逐模块
/// continue-and-collect，与 `migrate_collect` 同形；`file` 只出**文件名**（不含
/// 绝对路径，§6.8 信息泄漏面收敛）。
#[derive(Debug, serde::Serialize)]
pub struct SeedResult {
    pub module: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stmts: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次收集产物：模块名 + 文件名（诊断用）+ 文本。门禁与执行共用**同一份文本**
/// （§6.5「同一 chunk 既判又跑」），且避免同进程二次读盘的 TOCTOU。
struct SeedText {
    module: String,
    label: String,
    text: String,
}

/// seed 错误文本脱敏 + 限长：`sanitize_dsn` 抹 DSN 凭据、按 `log_snip` 口径裁到
/// 300 字，避免把绝对路径 / 完整 SQL 回显到 JSON / stderr（§6.8）。
fn seed_err(e: impl std::fmt::Display) -> String {
    let s = crate::migrate_cmd::sanitize_dsn(&e.to_string());
    s.chars().take(300).collect()
}

/// 收齐全部模块的 seed 文本（`pick` 选方言件/通用件、绝不双跑）；模块无匹配
/// seed → 跳过（no-op，对齐冷启动纪律：不告警、零日志噪音）。
fn collect_texts(modules: &[(String, PathBuf)], dialect: Dialect) -> Result<Vec<SeedText>, String> {
    let mut out = Vec::new();
    for (name, root) in modules {
        let p = pick(root, dialect);
        let Some(f) = p.chosen else {
            continue;
        };
        let label = f
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| show(&f));
        let text = std::fs::read_to_string(&f)
            .map_err(|e| format!("读取 seed {name}/{label} 失败：{}", seed_err(e)))?;
        out.push(SeedText {
            module: name.clone(),
            label,
            text,
        });
    }
    Ok(out)
}

/// 全量预扫门禁（FR11）：收齐的**全部** chunk 统一过 fail-closed 门禁后，调用方
/// 才进 S002 与 exec——命中即零行落库，杜绝「A 已提交、B 才命中 DDL」。
fn gate_texts(texts: &[SeedText], dialect: Dialect) -> Result<(), String> {
    for st in texts {
        for (i, stmt) in split_statements(&st.text).into_iter().enumerate() {
            if let Err(reason) = crate::checks::seed_stmt_allowed(stmt, dialect) {
                return Err(format!(
                    "seed {}/{} 第 {i} 条语句被 --with-seed 运行期门禁拒绝：{reason}\n  \
                     下一步：把 seed.sql 改成幂等 INSERT 后重跑 oj migrate --with-seed；\
                     或去掉 --with-seed，交由下一次冷启动重放",
                    st.module, st.label
                ));
            }
        }
    }
    Ok(())
}

/// 运行期门禁的**纯前置入口**（FR10，评审 A-major）：只收集 + 门禁、不执行，
/// 不带任何 policy 参数。与 [`replay_modules`] 共用 [`collect_texts`] / [`gate_texts`]，
/// 供单测直接钉死击穿尝试（评审：插旗式声明会漏防护）。
pub fn pre_validate_seeds(modules: &[(String, PathBuf)], dialect: Dialect) -> Result<(), String> {
    gate_texts(&collect_texts(modules, dialect)?, dialect)
}

/// `oj migrate --with-seed` 的 seed 重放入口：对**同一批 `s.modules`**、共用
/// `s.target` 库连接。内部由 `dialect_info()` → `Dialect::known` 解析方言（fail-loud，
/// 不复制两份门控，评审 E-B6），先全量门禁 + S002（零副作用）才逐语句 exec、
/// 逐模块 continue-and-collect。
///
/// `Ok(results)`：执行完成，逐模块 `ok/error`（任一 `ok=false` 由调用方折成命令
/// 非零退出）。`Err(e)`：执行前的**全局**失败（未知方言 / 门禁 / S002 / 读文件），
/// 此时零语句落库——与「单点 Err 吞掉逐模块结果」不同，逐模块失败仍在 `Ok` 内，
/// 故 §8 的 `seed.results[].ok=false` 不受影响。
pub async fn replay_modules(
    db: &Arc<dyn DataAccessor>,
    modules: &[(String, PathBuf)],
) -> Result<Vec<SeedResult>, String> {
    // 方言门控（T8，与 replay_all 同源）：未知方言 fail-loud，绝不回落 Sqlite。
    let dialect_name = db.dialect_info().name.clone();
    let Some(dialect) = Dialect::known(&dialect_name) else {
        return Err(only_js::bridge::db_error::unsupported(&dialect_name, "seed").message);
    };
    let texts = collect_texts(modules, dialect)?;
    // 全量预扫：门禁 + S002 都跑在 exec 之前（FR11 零副作用）。
    gate_texts(&texts, dialect)?;
    // S002 冲突检查（选中集内）：表 → 首见文件。用 owned String 避免冷启动那份
    // `leak_str` 的进程级泄漏；语义一致：表→模块 单射，不静默合并。
    let mut owner: HashMap<String, String> = HashMap::new();
    for st in &texts {
        for t in create_tables(&st.text) {
            if let Some(prev) = owner.get(&t) {
                return Err(format!(
                    "S002: 表 `{t}` 被多处建表：{prev} 与 {}/{}\n  \
                     原因：模块自治要求 表→模块 单射（§8-1），不静默合并。\n  \
                     下一步：保留唯一归属处的建表与数据，删除另一处后重试。",
                    st.module, st.label
                ));
            }
            owner.insert(t, st.label.clone());
        }
    }
    let mut results = Vec::new();
    for st in &texts {
        let mut applied = 0usize;
        let mut failure = None;
        for (i, stmt) in split_statements(&st.text).into_iter().enumerate() {
            // 门禁判原文、执行判改写后（§6.5 同一 chunk 既判又跑）。
            let sql = translate_insert(stmt, dialect);
            if let Err(e) = db.exec_with_params(&sql, &[]).await {
                let msg = seed_err(e);
                tracing::error!(
                    module = %st.module,
                    file = %st.label,
                    seq = i,
                    "seed failed: {msg}"
                );
                failure = Some(format!("第 {i} 条语句执行失败：{msg}"));
                break; // 模块内首错即停（与迁移模块内 abort 同形）
            }
            applied += 1;
        }
        match failure {
            None => {
                tracing::info!(
                    module = %st.module,
                    file = %st.label,
                    dialect = dialect.as_str(),
                    stmts = applied,
                    "seed ok"
                );
                results.push(SeedResult {
                    module: st.module.clone(),
                    ok: true,
                    file: Some(st.label.clone()),
                    dialect: Some(dialect.as_str().to_string()),
                    stmts: Some(applied),
                    error: None,
                });
            }
            Some(error) => results.push(SeedResult {
                module: st.module.clone(),
                ok: false,
                file: Some(st.label.clone()),
                dialect: Some(dialect.as_str().to_string()),
                stmts: None,
                error: Some(error),
            }),
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given oracle 方言 When replay_all 入口门控（seed）
    /// Then fail-loud：Dialect::known 不接受 oracle，unsupported 文本点名方言与能力名 seed。
    /// （与 migrate 同形：直接测门控 predicate，避免跨 crate mock accessor 的 async_trait
    /// 生命期陷阱。）
    #[test]
    fn oracle_seed_is_unsupported() {
        let name = "oracle";
        assert!(
            Dialect::known(name).is_none(),
            "oracle 不应在 builder-known 子集"
        );
        let err = only_js::bridge::db_error::unsupported(name, "seed").message;
        assert!(err.contains("oracle"), "{err}");
        assert!(err.contains("seed"), "{err}");
    }

    #[test]
    fn create_tables_extracts_names() {
        // 基本形态：IF NOT EXISTS / 大小写混合 / 多语句
        assert_eq!(
            create_tables("CREATE TABLE IF NOT EXISTS a (id INTEGER);\ncreate table B( id text );"),
            vec!["a", "B"]
        );
        // 引号三形态 + schema 限定取末段
        assert_eq!(
            create_tables(
                "CREATE TABLE \"q1\" (x); CREATE TABLE `q2` (x); CREATE TABLE [q3] (x); \
                           CREATE TABLE main.qualified (x); CREATE TABLE \"main\".\"qd\" (x);"
            ),
            vec!["q1", "q2", "q3", "qualified", "qd"]
        );
        // INSERT / CREATE INDEX / 注释行不算
        assert_eq!(
            create_tables(
                "-- CREATE TABLE commented (x);\nINSERT INTO t (x) VALUES (1);\nCREATE INDEX idx ON t (x);"
            ),
            Vec::<String>::new()
        );
        // 空文本 / 垃圾输入不 panic
        assert_eq!(create_tables(""), Vec::<String>::new());
        assert_eq!(create_tables("create table"), Vec::<String>::new());
    }

    #[test]
    fn split_keeps_inheritance_of_semicolon_rule() {
        assert_eq!(split_statements("a; b;;"), vec!["a", "b"]);
        assert!(split_statements("  ;; ").is_empty());
    }

    #[test]
    fn translate_insert_rewrites_per_dialect() {
        let s = "INSERT OR IGNORE INTO t (a) VALUES (1)";
        assert_eq!(translate_insert(s, Dialect::Sqlite), s);
        assert_eq!(
            translate_insert(s, Dialect::MySql),
            "INSERT IGNORE INTO t (a) VALUES (1)"
        );
        assert_eq!(
            translate_insert(s, Dialect::Postgres),
            "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING"
        );
        // 大小写不敏感；保留作者原文的其余部分。
        assert_eq!(
            translate_insert("insert or ignore into t values (1)", Dialect::MySql),
            "INSERT IGNORE into t values (1)"
        );
        // 非 INSERT / 其他幂等形态 / 无词边界 → 原样透传。
        assert_eq!(
            translate_insert("UPDATE t SET a = 1", Dialect::Postgres),
            "UPDATE t SET a = 1"
        );
        assert_eq!(
            translate_insert("INSERT INTO t VALUES (1)", Dialect::MySql),
            "INSERT INTO t VALUES (1)"
        );
        assert_eq!(
            translate_insert("INSERT OR REPLACE INTO t VALUES (1)", Dialect::Postgres),
            "INSERT OR REPLACE INTO t VALUES (1)"
        );
        assert_eq!(
            translate_insert("INSERT OR IGNORED INTO t VALUES (1)", Dialect::MySql),
            "INSERT OR IGNORED INTO t VALUES (1)"
        );
    }

    /// 前置注释行不得绕过方言改写（F2 回归钉子：v0.1.65 中 `--` 注释打头的
    /// chunk 让 `INSERT OR IGNORE` 原样下发 mysql → 1064）。
    #[test]
    fn translate_insert_strips_leading_comments() {
        // 注释行 + INSERT OR IGNORE：mysql / pg 均正确改写（注释剥除）。
        assert_eq!(
            translate_insert(
                "-- 引导数据\nINSERT OR IGNORE INTO t VALUES (1)",
                Dialect::MySql
            ),
            "INSERT IGNORE INTO t VALUES (1)"
        );
        assert_eq!(
            translate_insert(
                "-- a\n\n   -- b\nINSERT OR IGNORE INTO t (a) VALUES (1)",
                Dialect::Postgres
            ),
            "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING"
        );
        // 未命中改写：原文（含注释）原样透传，不动作者内容。
        assert_eq!(
            translate_insert("-- c\nINSERT INTO t VALUES (1)", Dialect::MySql),
            "-- c\nINSERT INTO t VALUES (1)"
        );
        // 尾随注释保留（只剥前置）。
        assert_eq!(
            translate_insert("INSERT OR IGNORE INTO t VALUES (1) -- c", Dialect::MySql),
            "INSERT IGNORE INTO t VALUES (1) -- c"
        );
        // 纯注释 chunk：不改写、不 panic。
        assert_eq!(
            translate_insert("-- c1\n-- c2", Dialect::MySql),
            "-- c1\n-- c2"
        );
        // 前缀 `--` 一律按注释剥（宽松，对齐 sqlite 语义；mysql 下 `--c` 非法 SQL，
        // 剥除后改写反而给出可执行的幂等语句）。
        assert_eq!(
            translate_insert("--c\nINSERT OR IGNORE INTO t VALUES (1)", Dialect::MySql),
            "INSERT IGNORE INTO t VALUES (1)"
        );
    }

    /// 测试辅助：真 sqlite 内存库 + 项目夹具，跑 replay_all 后回读建表清单。
    async fn replay(root: &Path, dir: &Path) -> Result<Vec<serde_json::Value>, String> {
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        let db = reg
            .connect("sqlite::memory:", root)
            .await
            .map_err(|e| e.to_string())?;
        replay_all(Some(&db), dir, None).await?;
        db.query_with_params(
            "select name from sqlite_master where type='table' order by name",
            &[],
        )
        .await
        .map_err(|e| e.to_string())
    }

    fn write(p: PathBuf, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let t = std::env::temp_dir().join(format!("oj-seed-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        t
    }

    /// 方言件存在即胜出：通用件里的建表**不得**执行（证明确实没双跑）。
    #[tokio::test(flavor = "current_thread")]
    async fn dialect_seed_wins_over_generic() {
        let t = tmp("win");
        write(t.join("src/u/seed.sql"), "CREATE TABLE generic_t (x);");
        write(
            t.join("src/u/seed.sqlite.sql"),
            "CREATE TABLE specific_t (x);",
        );
        let tables = replay(&t, &t.join("src")).await.unwrap();
        let names: Vec<&str> = tables.iter().filter_map(|r| r["name"].as_str()).collect();
        assert_eq!(names, vec!["specific_t"], "{names:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 没有通用件时，方言件单独可用。
    #[tokio::test(flavor = "current_thread")]
    async fn dialect_seed_runs_without_generic() {
        let t = tmp("solo");
        write(t.join("src/u/seed.sqlite.sql"), "CREATE TABLE only_t (x);");
        let tables = replay(&t, &t.join("src")).await.unwrap();
        let names: Vec<&str> = tables.iter().filter_map(|r| r["name"].as_str()).collect();
        assert_eq!(names, vec!["only_t"], "{names:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 只有别的方言件 → 该模块本轮无种子，且**报告**出来（起服 warn 的来源）。
    #[test]
    fn unmatched_dialect_seed_is_reported_not_silent() {
        let t = tmp("miss");
        write(
            t.join("src/u/seed.mysql.sql"),
            "INSERT IGNORE INTO t VALUES (1);",
        );
        let plan = collect(&t.join("src"), None, Dialect::Sqlite);
        assert!(plan.files.is_empty(), "{:?}", plan.files);
        assert_eq!(plan.skipped.len(), 1, "{:?}", plan.skipped);
        assert_eq!(plan.skipped[0].0, "u");
        assert!(plan.skipped[0].1.ends_with("seed.mysql.sql"));
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 两份别的方言件并存 → 都要报（不能只说「只有 mysql」）。
    #[test]
    fn all_mismatched_dialect_seeds_are_reported() {
        let t = tmp("miss2");
        write(
            t.join("src/u/seed.mysql.sql"),
            "INSERT IGNORE INTO t VALUES (1);",
        );
        write(t.join("src/u/seed.pg.sql"), "INSERT INTO t VALUES (1);");
        let plan = collect(&t.join("src"), None, Dialect::Sqlite);
        let files: Vec<String> = plan
            .skipped
            .iter()
            .map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["seed.mysql.sql", "seed.pg.sql"]);
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 标签写错（`postgres` 而非 `pg`）或大小写不符 → 永远不会被执行，必须点名。
    #[test]
    fn misnamed_dialect_tag_is_reported() {
        let t = tmp("tag");
        write(
            t.join("src/u/seed.postgres.sql"),
            "INSERT OR IGNORE INTO t VALUES (1);",
        );
        write(
            t.join("src/u/SEED.MYSQL.SQL"),
            "INSERT IGNORE INTO t VALUES (1);",
        );
        let plan = collect(&t.join("src"), None, Dialect::Postgres);
        assert!(plan.files.is_empty(), "{:?}", plan.files);
        let tags: Vec<&str> = plan
            .misnamed
            .iter()
            .map(|(_, tag, _)| tag.as_str())
            .collect();
        assert!(tags.contains(&"postgres"), "{tags:?}");
        assert!(tags.contains(&"MYSQL"), "{tags:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 零 seed 的工程不该因为没配 default 库而告警（any_seed 是那条告警的闸门）。
    #[test]
    fn any_seed_is_false_without_any_candidate() {
        let t = tmp("anys");
        std::fs::create_dir_all(t.join("src/u")).unwrap();
        assert!(!any_seed(&t.join("src"), None));
        write(t.join("src/u/seed.sql"), "x;");
        assert!(any_seed(&t.join("src"), None));
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 命名边界：`.bak` 后缀与目录不算 seed；`Seed.Sql` / `seed..sql` 不 panic，
    /// 且都不进候选（`scan` 按文件名精确匹配，行为跨平台一致）。
    #[test]
    fn scan_ignores_non_seed_names_and_never_panics() {
        let t = tmp("scan-edge");
        std::fs::create_dir_all(t.join("src/u/seed.foo.sql")).unwrap();
        write(t.join("src/u/seed.sql.bak"), "x;");
        write(t.join("src/u/seed.mysql.sql.bak"), "x;");
        write(t.join("src/u/Seed.Sql"), "x;");
        write(t.join("src/u/seed..sql"), "x;");
        let set = scan(&t.join("src/u"));
        assert!(set.candidates.is_empty(), "{:?}", set.candidates);
        let tags: Vec<&str> = set.misnamed.iter().map(|(tag, _)| tag.as_str()).collect();
        assert_eq!(tags, vec!["", ""], "{tags:?}"); // 仅两个大小写/空标签变体被点名
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 候选清单只列实际存在的文件，四类名一次给全（S003/S006 与运行时共用）。
    #[test]
    fn seed_candidates_list_existing_variants() {
        let t = tmp("cand");
        write(t.join("src/u/seed.sql"), "x;");
        write(t.join("src/u/seed.pg.sql"), "x;");
        let names: Vec<String> = seed_candidates(&t.join("src/u"))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["seed.sql", "seed.pg.sql"]);
        assert!(seed_candidates(&t.join("src/none")).is_empty());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replays_modules_in_order() {
        let t = std::env::temp_dir().join(format!("oj-seed-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        // 两个模块（user 建表插入；order 的 seed 引用 user 的表 → 模块顺序生效）
        write(
            t.join("src/user/seed.sql"),
            "CREATE TABLE IF NOT EXISTS account (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\
             \nINSERT OR IGNORE INTO account (id, name) VALUES (1, 'neo');",
        );
        write(
            t.join("src/order/seed.sql"),
            "CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY, account_id INTEGER);\
             \nINSERT OR IGNORE INTO orders VALUES (1, 1);",
        );
        let tables = replay(&t, &t.join("src")).await.unwrap();
        let names: Vec<&str> = tables.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            names.contains(&"account") && names.contains(&"orders"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_default_db_skips_with_no_error() {
        let t = std::env::temp_dir().join(format!("oj-seed-dial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(t.join("src/u/seed.sql"), "CREATE TABLE r (x);");
        // 无 default 库 → warn 跳过（有库时三方言都重放，无方言豁免）。
        replay_all(None, &t, None).await.unwrap();
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s002_module_vs_module_conflict() {
        let t = std::env::temp_dir().join(format!("oj-seed-s002b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        write(
            t.join("src/a/seed.sql"),
            "CREATE TABLE IF NOT EXISTS shared (id);",
        );
        write(t.join("src/b/seed.sql"), "create table shared (id);");
        let e = replay(&t, &t.join("src")).await.unwrap_err();
        assert!(
            e.contains("S002") && e.contains("a/seed.sql") && e.contains("b/seed.sql"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_seeds_is_silent_ok() {
        let t = std::env::temp_dir().join(format!("oj-seed-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/u")).unwrap();
        let tables = replay(&t, &t.join("src")).await.unwrap();
        assert!(tables.is_empty());
        let _ = std::fs::remove_dir_all(&t);
    }

    // ===== `oj migrate --with-seed`（v0.1.125）新增用例 ==========================

    /// 纯前置门禁入口能独立钉死 `/*x*/ DROP` 击穿（评审：插旗式声明会漏防护），
    /// 且合法幂等件放行、错误给下一步。
    #[test]
    fn pre_validate_seeds_blocks_bypass_and_accepts_idempotent() {
        let t = tmp("preval");
        let mods = vec![("u".to_string(), t.join("src/u"))];
        write(t.join("src/u/seed.sql"), "/*x*/ DROP TABLE t;");
        let e = pre_validate_seeds(&mods, Dialect::Sqlite).unwrap_err();
        assert!(e.contains("门禁"), "{e}");
        assert!(e.contains("下一步"), "错误须给下一步：{e}");
        write(
            t.join("src/u/seed.sql"),
            "INSERT OR IGNORE INTO t VALUES (1);",
        );
        assert!(pre_validate_seeds(&mods, Dialect::Sqlite).is_ok());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 无 seed 的模块 → 成功 no-op（FR4 零噪音）。
    #[test]
    fn pre_validate_seeds_no_seed_is_noop() {
        let t = tmp("noseed");
        std::fs::create_dir_all(t.join("src/u")).unwrap();
        let mods = vec![("u".to_string(), t.join("src/u"))];
        assert!(pre_validate_seeds(&mods, Dialect::Sqlite).is_ok());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 直连真 sqlite 内存库（供 replay_modules 直测，绕开 config/插件装配）。
    async fn seed_db(t: &Path) -> Arc<dyn DataAccessor> {
        only_js::bridge::DbBackendRegistry::builtin()
            .connect("sqlite::memory:", t)
            .await
            .unwrap()
    }

    /// FR11 全量预扫零副作用（击穿尝试）：模块 A 合法 + 模块 B 含 DDL → A 也零行
    /// 落库，杜绝「A 已提交、B 才命中门禁」。
    #[tokio::test(flavor = "current_thread")]
    async fn replay_modules_full_prescan_leaves_zero_rows() {
        let t = tmp("prescan");
        write(
            t.join("src/a/seed.sql"),
            "INSERT OR IGNORE INTO g VALUES (1);",
        );
        write(t.join("src/b/seed.sql"), "CREATE TABLE leaked (x);");
        let db = seed_db(&t).await;
        db.exec_with_params("CREATE TABLE g (x)", &[])
            .await
            .unwrap();
        let mods = vec![
            ("a".to_string(), t.join("src/a")),
            ("b".to_string(), t.join("src/b")),
        ];
        let e = replay_modules(&db, &mods).await.unwrap_err();
        assert!(e.contains("门禁"), "B 的 DDL 须被门禁拒：{e}");
        // A 的合法 INSERT 也不得落库（全量预扫在 exec 之前）。
        let rows = db.query("select count(*) as n from g").await.unwrap();
        assert_eq!(rows[0]["n"].as_i64().unwrap(), 0, "全量预扫须零行落库");
        let leaked = db
            .query("select name from sqlite_master where type='table' and name='leaked'")
            .await
            .unwrap();
        assert!(leaked.is_empty(), "DDL 不得建表");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 重复 replay_modules 行数不翻倍（§9 #2）；结果形状 file 只出文件名。
    #[tokio::test(flavor = "current_thread")]
    async fn replay_modules_is_idempotent_and_shapes_result() {
        let t = tmp("idem");
        // OR IGNORE 需要主键 / 唯一约束才真正幂等；无约束时每次 INSERT 都成功。
        write(
            t.join("src/u/seed.sql"),
            "INSERT OR IGNORE INTO g VALUES (1, 'a');\nINSERT OR IGNORE INTO g VALUES (2, 'b');",
        );
        let db = seed_db(&t).await;
        db.exec_with_params("CREATE TABLE g (id INTEGER PRIMARY KEY, x TEXT)", &[])
            .await
            .unwrap();
        let mods = vec![("u".to_string(), t.join("src/u"))];
        let r1 = replay_modules(&db, &mods).await.unwrap();
        assert_eq!(r1.len(), 1);
        assert!(r1[0].ok);
        assert_eq!(r1[0].stmts, Some(2));
        assert_eq!(r1[0].file.as_deref(), Some("seed.sql"));
        assert_eq!(r1[0].dialect.as_deref(), Some("sqlite"));
        replay_modules(&db, &mods).await.unwrap();
        let rows = db.query("select count(*) as n from g").await.unwrap();
        assert_eq!(rows[0]["n"].as_i64().unwrap(), 2, "重放不得翻倍");
        let _ = std::fs::remove_dir_all(&t);
    }
}
