//! T14 e2e 共用例链（达梦远程主腿 / 金仓可选腿共用；各库测试文件只做 env 门槛 +
//! 驱动 jar 归置后调 [`run_chain`]。探活纪律（T1 评审钉）：一切用例断言落在错误码 /
//! 能力字段 / 类型形态上，**不锚消息文本**；环境差异先探测、打印实际形态再钉死。
//!
//! 探活阶段：describe() 能力串（stream/cancel 缺席 → skip 原因打印，用例链整体不跑）；
//! CASE_SENSITIVE 元数据行为（引号混合大小写列名回读）。旗舰裁决：
//!   a. 真·在途 cancel（DM 是真流式驱动，H2 embedded  leg 到不了的分支）；
//!   b. cancel 后 stream_close 的拆解语义（Ok(ok) 分支 vs "unknown stream_id" 吞咽分支
//!      ——裁定插件 err-吞咽分支对真驱动是活代码还是死代码）。
//!
//! 同步纪律：drive 有界 deadline 轮询，无固定 sleep。

use oj_db_jdbc::sidecar::SidecarCfg;
use oj_db_jdbc::{__test_init, vt};
use oj_plugin_ffi::{FfiFuture, RString};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 单帧驱动等待上界（真 JVM + 真网络库余量），是 deadline 不是同步手段。
const BOUND: Duration = Duration::from_secs(60);
pub fn v(b: &[u8]) -> Value {
    serde_json::from_slice(b).expect("响应应为合法 JSON")
}

pub fn assert2(cond: bool, what: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("assert failed: {what}"))
    }
}

/// FfiFuture → 测试异步桥（与 tests/h2_smoke.rs 同款有界轮询）。
pub async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + BOUND;
    loop {
        match (fut.poll)(fut.state) {
            0 => {
                if Instant::now() >= deadline {
                    (fut.free)(fut.state);
                    fut.state = std::ptr::null_mut();
                    return Err("ffi drive timeout".into());
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            code => {
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut();
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Err(e)) => Err(e[..].to_string()),
                    _ => Err("ffi poll reported error but take succeeded".into()),
                };
            }
        }
    }
}

/// select count(*) 首行 n 列（事务可见性断言的共用件）。列别名带引号保小写
/// （各厂未引标识符大小写策略不一，引号是跨厂稳定键）。
async fn count(f: &mut FfiFuture) -> Result<i64, String> {
    let rows = v(&drive(f).await?);
    rows[0]["n"]
        .as_i64()
        .ok_or_else(|| "count 缺 n".to_string())
}

/// 夹具表名（大写未引标识符是 Oracle 系最稳形态；DROP 幂等，共享远程实例可重跑）。
const T_TYPES: &str = "OJ_E2E_T";
const T_CS: &str = "OJ_E2E_CS";
const T_S: &str = "OJ_E2E_S";
const T_LOCK: &str = "OJ_E2E_LOCK";

/// 跑前必填（各库测试文件从 env 组装）。
pub struct ChainInput {
    pub bridge: PathBuf,
    pub drivers: PathBuf,
    /// 完整 `jdbc:{...}` 信封 DSN（URL/凭据已内嵌）。
    pub dsn: String,
    /// 真口令（connect 失败脱敏断言用：错误里绝不允许出现）。
    pub password: String,
}

/// 空串探测结果（DM 系空串-or-null 差异按实际钉死，不强迫 H2 语义）。
#[derive(Debug, PartialEq, Eq)]
pub enum EmptyString {
    /// 回读为 JSON null（库把 '' 存成/读成 NULL）。
    AsNull,
    /// 回读为空串原样。
    AsEmpty,
}

/// 用例链主体。Ok(verdict_lines) = 全绿并回传裁决记录（测试文件负责打印）。
pub async fn run_chain(in_: &ChainInput) -> Result<Vec<String>, String> {
    __test_init(SidecarCfg {
        java_bin: None,
        java_opts: vec![],
        bridge_jar: in_.bridge.clone(),
        drivers_dir: in_.drivers.clone(),
        hello_timeout: Duration::from_secs(30),
        env: vec![],
        log: None,
    });
    let mut verdicts = Vec::new();

    // ---- 探活 0：describe() 能力串（stream/cancel 缺席 → 不跑用例链）----
    let d: Value =
        serde_json::from_str(&vt::describe()[..]).map_err(|e| format!("describe 非 JSON: {e}"))?;
    assert2(d["dialect"] == "jdbc", "describe.dialect 恒 \"jdbc\"")?;
    let caps = d["capabilities"]
        .as_array()
        .ok_or("describe 缺 capabilities")?;
    let has = |c: &str| caps.iter().any(|x| x == c);
    if !has("stream") || !has("cancel") {
        return Err(format!(
            "SKIP——驱动/插件组合不支持 stream/cancel（实际能力串 {caps:?}），用例链不跑"
        ));
    }
    // T1 契约前置：宿主 db.import 门控只看方言名，"jdbc" 不在 known 子集
    // （require_known_dialect → OJ_DB_UNSUPPORTED / capability "import"，T1 单测已钉），
    // 此处对真插件钉死触发门控的工厂描述符字段（dialect + 能力串），不锚消息文本。
    assert2(has("placeholder_q"), "capabilities 含 placeholder_q")?;
    println!("e2e: describe = {d}");

    // ---- 探活 1：connect 失败脱敏（计划钉：错误信封不得回显 user:pass@）----
    // 空口令时 replace("",…)/contains("") 无意义（处处命中），跳过该子用例。
    if in_.password.is_empty() {
        println!("e2e: 空口令——跳过 connect 脱敏子用例（replace/contains 对空串恒真）");
    } else {
        let bad = in_.dsn.replace(&in_.password, "OJ_E2E_WRONG_PW");
        let mut f = vt::connect(RString::from(bad.as_str()));
        let e = drive(&mut f).await.expect_err("错口令 connect 必须失败");
        assert2(
            !e.contains(&in_.password) && !e.contains("OJ_E2E_WRONG_PW"),
            &format!("connect 错误不得回显口令: {e}"),
        )?;
    }

    // ---- connect（真远程实例）----
    let mut f = vt::connect(RString::from(in_.dsn.as_str()));
    let h = v(&drive(&mut f).await?)["handle"]
        .as_u64()
        .ok_or("connect ok 缺 handle")?;

    // ---- DDL 阶段（独立 auto-commit 连接阶段，绝不在事务里：DM DDL 自提交，
    //      但 rollback 语义跨厂不一，夹具表必须不受用例链事务影响）----
    // drop：先试 IF EXISTS（DM/金仓均支持），失败退回裸 drop 并忽略错。
    for t in [T_TYPES, T_CS, T_S, T_LOCK] {
        let mut f = vt::exec(
            h,
            RString::from(format!("drop table if exists {t}").as_str()),
            RString::from("[]"),
        );
        if drive(&mut f).await.is_err() {
            let mut f = vt::exec(
                h,
                RString::from(format!("drop table {t}").as_str()),
                RString::from("[]"),
            );
            let _ = drive(&mut f).await;
        }
    }
    let ddls = [
        format!(
            "create table {T_TYPES} (id int, name varchar(64), note varchar(64), ts timestamp)"
        ),
        format!("create table {T_CS} (\"MiXeD\" int, lo int)"),
        format!("create table {T_S} (n int)"),
        format!("create table {T_LOCK} (id int primary key, v int)"),
    ];
    for ddl in &ddls {
        let mut f = vt::exec(h, RString::from(ddl.as_str()), RString::from("[]"));
        assert2(
            v(&drive(&mut f).await?) == json!(0),
            &format!("DDL 应成功且影响 0 行: {ddl}"),
        )?;
    }

    // ---- CASE_SENSITIVE 探活：引号保大小写 / 未引按厂策略 ----
    {
        let mut f = vt::exec(
            h,
            RString::from(format!("insert into {T_CS} (\"MiXeD\", lo) values (7, 8)").as_str()),
            RString::from("[]"),
        );
        assert2(v(&drive(&mut f).await?) == json!(1), "CS 探活 insert 1 行")?;
        let mut f = vt::query(
            h,
            RString::from(format!("select \"MiXeD\", lo from {T_CS}").as_str()),
            RString::from("[]"),
        );
        let row = v(&drive(&mut f).await?)[0].clone();
        let mixed = row
            .as_object()
            .and_then(|m| m.keys().find(|k| k.eq_ignore_ascii_case("mixed")).cloned())
            .ok_or("CS 探活缺 mixed 键")?;
        let lo = row
            .as_object()
            .and_then(|m| m.keys().find(|k| k.eq_ignore_ascii_case("lo")).cloned())
            .ok_or("CS 探活缺 lo 键")?;
        assert2(
            row[&mixed] == json!(7) && row[&lo] == json!(8),
            "CS 探活值往返",
        )?;
        println!("e2e: CASE_SENSITIVE 实际 = 引号列回读键 {mixed:?}，未引列回读键 {lo:?}");
        verdicts.push(format!("cs_probe: quoted={mixed} unquoted={lo}"));
    }

    // ---- 用例 1：insert 含 null 列（setNull(Types.VARCHAR) 通道）+ 整数列回 number
    //      （DM 把 INT 上报 NUMERIC，scale==0 走整数族）+ 时间值 UTC 口径 ----
    {
        let mut f = vt::exec(
            h,
            RString::from(
                format!("insert into {T_TYPES} (id, name, note, ts) values (?, ?, ?, timestamp '2026-01-02 03:04:05')").as_str(),
            ),
            RString::from(r#"[42,"hello",null]"#),
        );
        assert2(v(&drive(&mut f).await?) == json!(1), "类型用例 insert 1 行")?;
        let mut f = vt::query(
            h,
            RString::from(
                format!("select id, name, note, ts from {T_TYPES} where id = 42").as_str(),
            ),
            RString::from("[]"),
        );
        let row = v(&drive(&mut f).await?)[0].clone();
        // 整数列（NUMERIC scale==0）必须回 JSON number，不是字符串。
        assert2(
            row["id"] == json!(42),
            &format!("整数列回 number: {}", row["id"]),
        )?;
        assert2(row["name"] == json!("hello"), "name 往返")?;
        // null 绑参（setNull VARCHAR）必须回 JSON null（不是 ""、不是缺键）。
        assert2(
            row.get("note") == Some(&Value::Null),
            &format!("null 列回 JSON null: {}", row["note"]),
        )?;
        // 时间值 UTC 口径（-Duser.timezone=UTC；Timestamp.toInstant → Z 尾恒 UTC）。
        assert2(
            row["ts"] == json!("2026-01-02T03:04:05Z"),
            &format!("ts UTC 口径: {}", row["ts"]),
        )?;
    }

    // ---- 用例 2：空串实际行为探测并钉死（DM 系差异按实际，不强迫 H2 语义）----
    let empty = {
        let mut f = vt::exec(
            h,
            RString::from(format!("insert into {T_TYPES} (id, name) values (43, '')").as_str()),
            RString::from("[]"),
        );
        assert2(v(&drive(&mut f).await?) == json!(1), "空串 insert 1 行")?;
        let mut f = vt::query(
            h,
            RString::from(format!("select name from {T_TYPES} where id = 43").as_str()),
            RString::from("[]"),
        );
        let cell = v(&drive(&mut f).await?)[0]["name"].clone();
        let r = match &cell {
            Value::Null => EmptyString::AsNull,
            Value::String(s) if s.is_empty() => EmptyString::AsEmpty,
            other => return Err(format!("空串回读非预期形态: {other}")),
        };
        println!("e2e: 空串实际行为 = {r:?}");
        verdicts.push(format!("empty_string: {r:?}"));
        r
    };
    let _ = empty; // 形态已钉死在上面的严格断言里；差异记录进 verdicts。

    // ---- 用例 3：begin/insert/rollback 不留痕；再 begin/insert/commit 留痕 ----
    {
        let base = {
            let mut f = vt::query(
                h,
                RString::from(format!("select count(*) as \"n\" from {T_TYPES}").as_str()),
                RString::from("[]"),
            );
            count(&mut f).await?
        };
        let mut f = vt::begin(h);
        let tx1 = v(&drive(&mut f).await?)["tx_id"]
            .as_u64()
            .ok_or("begin 缺 tx_id")?;
        let mut f = vt::tx_exec(
            h,
            tx1,
            RString::from(format!("insert into {T_TYPES} (id) values (900)").as_str()),
            RString::from("[]"),
        );
        assert2(v(&drive(&mut f).await?) == json!(1), "tx_exec insert 1")?;
        let mut f = vt::tx_rollback(h, tx1);
        assert2(v(&drive(&mut f).await?) == json!({}), "tx_rollback ok")?;
        let mut f = vt::query(
            h,
            RString::from(format!("select count(*) as \"n\" from {T_TYPES}").as_str()),
            RString::from("[]"),
        );
        assert2(
            count(&mut f).await? == base,
            "rollback 后行数复原（DDL 夹具在事务外）",
        )?;
        let mut f = vt::begin(h);
        let tx2 = v(&drive(&mut f).await?)["tx_id"]
            .as_u64()
            .ok_or("begin2 缺 tx_id")?;
        let mut f = vt::tx_exec(
            h,
            tx2,
            RString::from(format!("insert into {T_TYPES} (id) values (901)").as_str()),
            RString::from("[]"),
        );
        drive(&mut f).await?;
        let mut f = vt::tx_commit(h, tx2);
        assert2(v(&drive(&mut f).await?) == json!({}), "tx_commit ok")?;
        let mut f = vt::query(
            h,
            RString::from(format!("select count(*) as \"n\" from {T_TYPES}").as_str()),
            RString::from("[]"),
        );
        assert2(count(&mut f).await? == base + 1, "commit 后行数 +1")?;
        // 收尾夹具行，避免影响后续 count 断言的读者。
        let mut f = vt::exec(
            h,
            RString::from(format!("delete from {T_TYPES} where id in (900, 901)").as_str()),
            RString::from("[]"),
        );
        drive(&mut f).await?;
    }

    // ---- 用例 4：stream 批边界 250 行（100/100/50）+ done；close 幂等。
    //      本序列即宿主 db.export 泵消费的流式面（da.stream_query →
    //      stream_open/next×N/done/close），describe 能力串已断言 stream/cancel。----
    {
        let tuples: String = (0..250)
            .map(|n| format!("({n})"))
            .collect::<Vec<_>>()
            .join(",");
        let mut f = vt::exec(
            h,
            RString::from(format!("insert into {T_S} values {tuples}").as_str()),
            RString::from("[]"),
        );
        assert2(v(&drive(&mut f).await?) == json!(250), "stream 夹具 250 行")?;
        let mut f = vt::stream_open(
            h,
            RString::from(format!("select n from {T_S} order by n").as_str()),
            RString::from("[]"),
        );
        let sid = v(&drive(&mut f).await?)["stream_id"]
            .as_u64()
            .ok_or("stream_open 缺 stream_id")?;
        let mut batches: Vec<usize> = Vec::new();
        loop {
            let mut f = vt::stream_next(h, sid);
            let env = v(&drive(&mut f).await?);
            if env.get("done").and_then(Value::as_bool) == Some(true) {
                break;
            }
            let n = env["rows"].as_array().ok_or("next 信封缺 rows")?.len();
            batches.push(n);
            if batches.len() > 10 {
                return Err("批循环未收敛".into());
            }
        }
        assert2(
            batches == [100, 100, 50],
            &format!("批边界 100/100/50，实际 {batches:?}"),
        )?;
        let mut f = vt::stream_close(h, sid);
        assert2(v(&drive(&mut f).await?) == json!({}), "stream_close ok")?;
        let mut f = vt::stream_close(h, sid);
        assert2(v(&drive(&mut f).await?) == json!({}), "stream_close 幂等")?;
    }

    // ---- 用例 5/6（旗舰裁决 a/b）：真·在途 cancel 及其拆解语义 ----
    // 探活纪律：先竞速探测，驱动支持（未来版本/他厂）→ 严格断言 cancelled 信封 +
    // 活拆解裁决；不支持 → 如实打印 SKIP 原因，用例链其余照跑。
    //
    // DM 8.1.1.49 实测裁决（直接 JVM 探针 + e2e 双重证据）：
    // a. 在途 cancel 结构性不可达——
    //    1. JDBC 规范：cancel 仅对执行中的语句生效（批间空闲 no-op；H2 leg 已登记）；
    //    2. DM 的服务端计算全部发生在 executeQuery 期（SELECT-list 子查询被整体物化，
    //       实测 open 一次性算完全表）——而插件 API 在 open 返回前没有 stream_id，
    //       open 阶段 cancel 够不到（for update 锁亦在 open 即时求，同结论）；
    //    3. rs.next() 的在途时间几乎全是客户端传输（宽行夹具实测 ~18ms），而驱动的
    //       cancel 实现对语句/连接加锁——传输期 cancel 被串行化在 next 之后，落到
    //       已空闲的语句上成 no-op（12 批逐批竞速全部落败）；
    //    4. 唯一直接 JVM 探针观测到的 cancel 生效场景是 executeQuery 期服务端
    //       计算（DMException「操作被取消」）——如上，该窗口插件 API 结构不可达。
    //    ⇒ R7 cancelled 信封路径保持 fake-java 腿（T11）覆盖；真实 DM 下
    //      stream_cancel 是「被接受、无效果」（与 H2 no-op 语义同档），宿主
    //      协作式取消契约不受影响（cancel 置标，批间 next 照常，close 正常收口）。
    // b. 拆解语义（stream_close 的 err-吞咽分支死活）——真实 DM 造不出 cancelled
    //    流（见 a），活路径不可观测；按 Java 源码裁决：streamClose 是唯一摘除注册表
    //    者，cancel 不摘条目、disconnect 由插件侧同步摘除本地条目——故插件
    //    stream_close 的 "unknown stream_id" err-吞咽分支对任何驱动都是不可达的
    //    防御性死代码（防 Java 侧未来引入非 close 摘除路径的回归）。
    {
        // 夹具：1000 行（字面量多组 values）。
        let tuples: String = (1..=1000)
            .map(|n| format!("({n},{n})"))
            .collect::<Vec<_>>()
            .join(",");
        let mut f = vt::exec(
            h,
            RString::from(format!("insert into {T_LOCK} values {tuples}").as_str()),
            RString::from("[]"),
        );
        assert2(v(&drive(&mut f).await?) == json!(1000), "LOCK 夹具 1000 行")?;

        let mut f = vt::stream_open(
            h,
            RString::from(format!("select id from {T_LOCK} order by id").as_str()),
            RString::from("[]"),
        );
        let sid = v(&drive(&mut f).await?)["stream_id"]
            .as_u64()
            .ok_or("cancel 腿 open 缺 stream_id")?;

        // 逐批竞速探测：cancel 帧与 next 帧背靠背发出（槽函数即 dispatch，FfiFuture
        // poll 只是收割）。未来驱动若在途取消可达 → 首包 Err 即 cancelled 信封，
        // 严格断言并继续活拆解裁决；界内全是行/done → 按上方裁决打印 SKIP 原因。
        let mut cancelled: Option<String> = None;
        for _ in 0..6 {
            let mut next_fut = vt::stream_next(h, sid);
            let mut cancel_fut = vt::stream_cancel(h, sid);
            let (nres, cres) = tokio::join!(drive(&mut next_fut), drive(&mut cancel_fut));
            assert2(v(&cres?) == json!({}), "竞速 stream_cancel 被接受")?;
            match nres {
                Err(e) => {
                    cancelled = Some(e);
                    break;
                }
                Ok(b) => {
                    if v(&b).get("done").and_then(Value::as_bool) == Some(true) {
                        break;
                    }
                }
            }
        }

        match cancelled {
            Some(ce) => {
                // 未来驱动/他厂的可达路径：严格断言 + 活拆解裁决。
                let cv: Value = serde_json::from_str(&ce)
                    .map_err(|_| format!("cancelled Err 应为结构化信封: {ce}"))?;
                assert2(
                    cv["category"] == "cancelled",
                    &format!("cancelled 信封 category 字段: {ce}"),
                )?;
                let mut f = vt::stream_next(h, sid);
                let second = drive(&mut f)
                    .await
                    .expect_err("cancel 后第二次 next 必须 Err");
                let (kept, teardown_note) = if second.contains("unknown stream_id") {
                    (false, "Java 已拆注册表条目".to_string())
                } else {
                    let sv: Value = serde_json::from_str(&second)
                        .map_err(|_| format!("第二次 next Err 非结构化: {second}"))?;
                    assert2(
                        sv["category"] == "cancelled",
                        &format!("第二次 next 应为 cancelled 或 unknown stream_id: {second}"),
                    )?;
                    (true, "Java 保留注册表条目".to_string())
                };
                let mut f = vt::stream_close(h, sid);
                assert2(
                    v(&drive(&mut f).await?) == json!({}),
                    "cancel 后 stream_close ok",
                )?;
                let branch = if kept {
                    "close 走 Ok(ok) 实分支——err-吞咽分支对真驱动是死代码"
                } else {
                    "close 走 err-吞咽分支（活代码）"
                };
                println!("e2e: 拆解语义（活路径） = {teardown_note}；{branch}");
                verdicts.push(format!(
                    "teardown(live): kept={kept} ({teardown_note}; {branch})"
                ));
                verdicts.push("mid_fetch_cancel(live): reachable".to_string());
            }
            None => {
                let mut f = vt::stream_close(h, sid);
                assert2(
                    v(&drive(&mut f).await?) == json!({}),
                    "cancel 竞速探测后 stream_close ok",
                )?;
                println!(
                    "e2e: SKIP 在途 cancel 严格断言——DM 8.1.1.49 结构性不可达：服务端计算在 open 期（无 stream_id 可 cancel），传输期 cancel 被驱动串行化落成 no-op（逐批竞速全落败；直接 JVM 探针仅观测到 executeQuery 期「操作被取消」）。R7 信封路径由 fake-java 腿覆盖。"
                );
                println!(
                    "e2e: 拆解语义（源码裁决） = Java 仅 streamClose 摘除注册表条目，cancel/disconnect 均不经该 err——插件 unknown-stream-id 吞咽分支对真驱动是不可达的防御性死代码。"
                );
                verdicts.push(
                    "mid_fetch_cancel: UNREACHABLE on DM 8.1.1.49 (open-phase compute; transfer-phase cancel serialized to no-op; R7 covered by fake-java leg)"
                        .to_string(),
                );
                verdicts.push(
                    "teardown: DEAD CODE by inspection (Java removes entry only in streamClose; live path unobservable because mid-fetch cancel unreachable)"
                        .to_string(),
                );
            }
        }
    }

    // ---- 收尾：清理夹具表 + 关闭连接 ----
    for t in [T_TYPES, T_CS, T_S, T_LOCK] {
        let mut f = vt::exec(
            h,
            RString::from(format!("drop table if exists {t}").as_str()),
            RString::from("[]"),
        );
        let _ = drive(&mut f).await;
    }
    vt::close(h);
    let mut f = vt::query(h, RString::from("select 1"), RString::from("[]"));
    let e = drive(&mut f)
        .await
        .expect_err("close 后查询必须报 unknown handle");
    assert2(e.contains("unknown handle"), &format!("实际: {e}"))?;

    Ok(verdicts)
}

/// PATH/JAVA_HOME 粗解析 java（与 SidecarMgr 的解析顺序同序，仅为诚实 skip 判定）。
pub fn java_available() -> bool {
    if let Some(jh) = std::env::var_os("JAVA_HOME")
        && Path::new(&jh).join("bin/java").exists()
    {
        return true;
    }
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
        .unwrap_or(false)
}
