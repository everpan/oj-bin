use super::*;
use crate::testutil::{drive, fwd, temp_dir};
use oj_plugin_ffi::{RBytes, RString};
use serde_json::Value;
use tokio::sync::Semaphore;

const PROFILE: &str = "default";

/// B6：`messageId` 只留队列号或截断 —— MTA 原始应答可能带服务器指纹/队列信息，
/// 而它会进信封并上送总线（脱敏面）。
#[test]
fn message_id_keeps_queue_id_only_or_truncates() {
    // Postfix 系：只留 `queued as` 之后的队列号（去尾部标点）。
    assert_eq!(
        sanitize_message_id("250 2.0.0 Ok: queued as 4Wx1AbC\r\n"),
        "4Wx1AbC"
    );
    assert_eq!(sanitize_message_id("250 OK queued as ABC123."), "ABC123");
    // FileTransport：落盘 id 原样（无空格、短）。
    assert_eq!(sanitize_message_id("mail-9f8a1b"), "mail-9f8a1b");
    // 其它应答：剥 CR/LF 并截断到 64。
    let long = format!("250 {}", "x".repeat(200));
    let got = sanitize_message_id(&long);
    assert_eq!(got.len(), MESSAGE_ID_MAX, "{got}");
    assert!(!got.contains('\n') && !got.contains('\r'), "{got}");
    assert_eq!(sanitize_message_id("250\r\nOK\r\n"), "250OK");
}

/// B2 纵深防御：附件超限在**插件侧**也回 `code:5`（宿主是主判据；这一层防第三方宿主
/// 漏判），且**不占队列槽位**（超限信不落盘 = 没进 worker）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_rejects_oversized_attachments_with_code5() {
    let dir = temp_dir("engine-attlimit");
    let cfg = MailConfig::parse(&format!(
        r#"{{"max_attachment_bytes":8,"max_total_attachment_bytes":12,
                 "default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,
                             "mechanism":"login","file_transport":"{}"}}}}"#,
        fwd(&dir)
    ))
    .expect("cfg");
    let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎");

    // 单附件超限（9 > 8）。
    let mut fut = eng.submit(
        PROFILE,
        &req_assemble("j-big", true),
        vec![att_bytes("a.bin", "application/octet-stream", b"123456789")],
    );
    let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
    assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("max_attachment_bytes"),
        "文案须给配置键与下一步: {v}"
    );

    // 合计超限（两件各 8 字节 = 16 > 12）。
    let req2 = json!({
        "from": "from@example.com",
        "to": ["to@example.com"],
        "text": "hi",
        "jobId": "j-big-2",
        "attachments": [
            {"filename": "a.bin", "blobKey": "k1"},
            {"filename": "b.bin", "blobKey": "k2"},
        ],
    })
    .to_string();
    let mut fut = eng.submit(
        PROFILE,
        &req2,
        vec![
            att_bytes("a.bin", "application/octet-stream", b"12345678"),
            att_bytes("b.bin", "application/octet-stream", b"12345678"),
        ],
    );
    let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
    assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("max_total_attachment_bytes"),
        "{v}"
    );

    // 未投递（file transport 目录里没有 .eml）。
    assert!(
        std::fs::read_dir(&dir).expect("目录").next().is_none(),
        "超限附件不得进 worker（不落盘）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// B3 兜底：未给 `jobId` 时引擎自造的 id 也不得是可枚举的 `{pid}-{seq}`
/// （正常路宿主恒注入 id，故这只是纵深防御；这里钉死形态与唯一性）。
#[test]
fn fallback_job_id_is_not_pid_enumerable() {
    let a = next_job_id();
    let b = next_job_id();
    assert_ne!(a, b, "进程内必须唯一");
    let (prefix, seq) = a.rsplit_once('-').expect("形态 <hex>-<seq>");
    assert_eq!(prefix.len(), 16, "{a}");
    assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    assert!(seq.parse::<u64>().is_ok(), "{a}");
    assert!(
        !a.starts_with(&format!("{}-", std::process::id())),
        "不得是可枚举的 pid 前缀：{a}"
    );
}

/// `raw` 路的最小合法 req（信封 + 原文）。
fn req_raw(job_id: Option<&str>, enqueue_only: bool) -> String {
    let mut v = json!({
        "from": "from@example.com",
        "to": ["to@example.com"],
        "raw": "Subject: t\r\n\r\nbody",
        "enqueue_only": enqueue_only,
    });
    if let Some(id) = job_id {
        v["jobId"] = json!(id);
    }
    v.to_string()
}

/// 结构化组装路的最小合法 req（**无** `raw`）；`attachment` 决定是否声明附件引用。
fn req_assemble(job_id: &str, attachment: bool) -> String {
    let mut v = json!({
        "from": "from@example.com",
        "to": ["to@example.com"],
        "subject": "组装主题",
        "text": "hi",
        "html": "<b>hi</b>",
        "jobId": job_id,
    });
    if attachment {
        v["attachments"] = json!([{ "filename": "a.pdf", "blobKey": "k1" }]);
    }
    v.to_string()
}

/// 宿主解析结果形态的附件（原始字节，非 base64）。
fn att_bytes(filename: &str, mime: &str, bytes: &[u8]) -> MailAttachment {
    MailAttachment {
        filename: RString::from(filename),
        mime: RString::from(mime),
        bytes: RBytes::from(bytes),
    }
}

/// file transport 引擎（免网络；`.eml` 落到 `dir`）。
fn file_engine(dir: &std::path::Path) -> MailEngine {
    let cfg = MailConfig::parse(&format!(
            r#"{{"workers":1,"queue_capacity":4,"default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login","file_transport":"{}"}}}}"#,
            fwd(dir)
        ))
        .expect("cfg");
    MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎")
}

/// 经队列真投递一封，读回 file transport 落盘的 `.eml` 原文（`messageId` = 文件名主干）。
async fn submit_and_read_eml(
    eng: &MailEngine,
    req: &str,
    atts: Vec<MailAttachment>,
    dir: &std::path::Path,
) -> String {
    let mut fut = eng.submit(PROFILE, req, atts);
    let out = drive(&mut fut).await.expect("submit 必须回结果信封");
    let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
    assert_eq!(v["code"], CODE_OK, "信封: {v}");
    let id = v["data"]["messageId"].as_str().expect("messageId");
    let eml = dir.join(format!("{id}.eml"));
    std::fs::read_to_string(&eml).unwrap_or_else(|e| panic!("读 {eml:?} 失败: {e}"))
}

/// 投递一封并回其失败信封（`code != 0` 的用例）。
async fn submit_expect_fail(eng: &MailEngine, req: &str, atts: Vec<MailAttachment>) -> Value {
    let mut fut = eng.submit(PROFILE, req, atts);
    let out = drive(&mut fut)
        .await
        .expect("校验失败也须回信封（不是 FFI Err）");
    let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
    assert_ne!(v["code"], CODE_OK, "必须失败，实际: {v}");
    v
}

fn targets_with(t: MailTarget) -> HashMap<String, MailTarget> {
    HashMap::from([(PROFILE.to_string(), t)])
}

/// 立即成功的注入桩（不连网）。
fn ok_target(timeout: Duration) -> MailTarget {
    MailTarget::async_only(
        timeout,
        Arc::new(|_e, _r| Box::pin(async { Ok("stub-message-id".to_string()) })),
    )
}

/// 收集器 sink：enqueue 路的结果上送可在此断言（无需真 HostContext）。
fn collector() -> (DeliverSink, mpsc::UnboundedReceiver<(String, Vec<u8>)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let sink = DeliverSink::new(move |topic: &str, payload: &[u8]| {
        let _ = tx.send((topic.to_string(), payload.to_vec()));
    });
    (sink, rx)
}

/// 可控「已开始投递」信号 + 闸门（0 许可）：投递函数在闸门放行前不返回 —— 完全确定，
/// 不依赖 sleep / 网络 / 真实 SMTP。
fn gated_target(timeout: Duration) -> (MailTarget, Arc<Semaphore>, mpsc::UnboundedReceiver<()>) {
    let gate = Arc::new(Semaphore::new(0));
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let g = Arc::clone(&gate);
    let target = MailTarget::async_only(
        timeout,
        Arc::new(move |_e, _r| {
            let g = Arc::clone(&g);
            let started = started_tx.clone();
            Box::pin(async move {
                let _ = started.send(()); // 通知测试：worker 已取走 job 并进入投递
                let permit = g.acquire().await.map_err(|e| format!("gate closed: {e}"))?;
                permit.forget();
                Ok("gated-message-id".to_string())
            })
        }),
    );
    (target, gate, started_rx)
}

/// TDD-1：普通 `submit` 经**有界队列 + worker** 真投递，future resolve 出 `code:0` 信封。
/// 用 file transport profile：断言信封 `messageId` 与落盘 `.eml` 对得上——证明「经队列
/// 真投递」而非短路成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_through_queue_resolves_with_envelope() {
    let dir = temp_dir("engine-queue");
    let cfg = MailConfig::parse(&format!(
            r#"{{"workers":2,"queue_capacity":8,"default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login","file_transport":"{}"}}}}"#,
            fwd(&dir)
        ))
        .expect("cfg");
    let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎");

    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-queue-1"), false), vec![]);
    let out = drive(&mut fut).await.expect("submit 必须回结果信封");
    let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
    assert_eq!(v["code"], CODE_OK, "信封: {v}");
    assert_eq!(v["data"]["jobId"], "j-queue-1");

    let message_id = v["data"]["messageId"].as_str().expect("messageId");
    assert!(
        dir.join(format!("{message_id}.eml")).exists(),
        "信封里的 messageId 必须对应真落盘的 .eml（证明真投递）"
    );
}

/// TDD-2：背压。worker 被闸门挂住 + 队列容量 1 → 第三个 `submit` **立即** `code:4`
/// （`try_send` 而非 `await send`，不冻结调用方）。
#[tokio::test(flavor = "multi_thread")]
async fn full_queue_returns_code4_without_blocking() {
    let (target, gate, mut started) = gated_target(Duration::from_secs(60));
    let (sink, mut delivered) = collector();
    let eng = MailEngine::with_targets(targets_with(target), 1, 1, sink).expect("引擎");

    // A：占住唯一的 worker（等 `started` 才继续 ⇒ 此刻 worker 已把 A 取走，队列为空）。
    let _ = eng.submit(PROFILE, &req_raw(Some("j-a"), true), vec![]);
    started.recv().await.expect("worker 应已开始投递 A");
    // B：占住队列唯一槽位。
    let _ = eng.submit(PROFILE, &req_raw(Some("j-b"), true), vec![]);
    assert_eq!(gate.available_permits(), 0);

    // C：队列已满 → 立即回 code:4（此处只测 `submit` 调用本身是否阻塞）。
    let t0 = Instant::now();
    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-c"), false), vec![]);
    let call_elapsed = t0.elapsed();
    assert!(
        call_elapsed < Duration::from_secs(2),
        "submit 必须非阻塞返回（闸门未放行也不得等待）：{call_elapsed:?}"
    );
    let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
    assert_eq!(v["code"], CODE_QUEUE_FULL, "信封: {v}");
    assert_eq!(v["data"]["jobId"], "j-c");
    // A6：文案与全链统一为中文（不再回英文 "queue full"），且指出下一步。
    let msg = v["msg"].as_str().unwrap_or_default();
    assert!(
        msg.contains("队列已满") && msg.contains("下一步"),
        "背压文案须中文且给下一步：{v}"
    );

    // 放行 A/B：队列被消费完 → drain 成功（也证明 A/B 确实在队列/在途，而非被丢弃）。
    gate.add_permits(2);
    assert!(eng.drain(Duration::from_secs(5)).is_ok(), "drain 应成功");
    assert_eq!(
        delivered.try_recv().expect("A 的结果上送").0,
        TOPIC_MAIL_RESULT
    );
    assert_eq!(
        delivered.try_recv().expect("B 的结果上送").0,
        TOPIC_MAIL_RESULT
    );
}

/// TDD-3：`enqueue_only` 立即回**统一信封** `{code:0,msg:"ok",data:{jobId}}`（**非**裸
/// `{jobId}` —— 三层公开契约都承诺 `res.data.jobId`）；真实完成经 sink 上送 `mail.result`
/// （信封含**同** jobId、code 0，且**不含**收件人/主题）。
#[tokio::test(flavor = "multi_thread")]
async fn enqueue_delivers_completion_via_sink() {
    let (sink, mut delivered) = collector();
    let eng = MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 8, sink)
        .expect("引擎");

    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-enq-1"), true), vec![]);
    let v: Value =
        serde_json::from_slice(&drive(&mut fut).await.expect("enqueue 回统一信封")).unwrap();
    // 严格形态：顶层只有 code/msg/data，data 只有 jobId（**没有**裸 `jobId` 顶层键）。
    let mut top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    top.sort_unstable();
    assert_eq!(top, ["code", "data", "msg"], "enqueue 必须是统一信封: {v}");
    assert_eq!(v["code"], CODE_OK, "enqueue 立即回执: {v}");
    assert_eq!(v["msg"], "ok", "{v}");
    assert_eq!(v["data"]["jobId"], "j-enq-1", "enqueue 立即回 jobId: {v}");
    assert!(
        v["data"].get("messageId").is_none(),
        "投递尚未发生，不得写 messageId: {v}"
    );

    let (topic, payload) = tokio::time::timeout(Duration::from_secs(5), delivered.recv())
        .await
        .expect("worker 完成后应上送结果")
        .expect("sink 未关闭");
    assert_eq!(topic, TOPIC_MAIL_RESULT);
    let v: Value = serde_json::from_slice(&payload).expect("上送是 JSON 信封");
    assert_eq!(v["code"], CODE_OK, "上送信封: {v}");
    assert_eq!(v["data"]["jobId"], "j-enq-1", "须带同一个 jobId: {v}");

    let text = String::from_utf8(payload).expect("UTF-8");
    for banned in ["to@example.com", "Subject", "subject"] {
        assert!(
            !text.contains(banned),
            "结果上送不得含收件人/主题（{banned}）: {text}"
        );
    }
    assert!(eng.drain(Duration::from_secs(5)).is_ok());
}

/// TDD-4：graceful drain —— 入队后立刻 `drain`，**在途 job 必须跑完**（结果上送 +
/// 全部 worker 退出），且全程无 panic / 无「no reactor running」类 abort
/// （transport 与 runtime 的销毁顺序见 `dispose`）。
#[tokio::test(flavor = "multi_thread")]
async fn drain_waits_inflight_then_stops() {
    let (sink, mut delivered) = collector();
    // 在途 job 需 50ms 才完成：drain 必须等它，而不是直接扔下 runtime。
    let slow = MailTarget::async_only(
        Duration::from_secs(5),
        Arc::new(|_e, _r| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok("drained-message-id".to_string())
            })
        }),
    );
    let eng = MailEngine::with_targets(targets_with(slow), 2, 8, sink).expect("引擎");

    let _ = eng.submit(PROFILE, &req_raw(Some("j-drain"), true), vec![]);
    eng.drain(Duration::from_secs(10))
        .expect("drain 应在超时内完成且 worker 全部退出");

    // drain 成功 ⇒ 在途 job 已完成并上送（上送发生在 worker 退出之前）。
    let (topic, payload) = delivered.try_recv().expect("在途 job 的结果应在停机前上送");
    assert_eq!(topic, TOPIC_MAIL_RESULT);
    let v: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(v["code"], CODE_OK, "信封: {v}");
    assert_eq!(v["data"]["jobId"], "j-drain");

    // 停机后 submit 必须 fail-loud（不再入队）。
    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-after"), false), vec![]);
    assert!(
        drive(&mut fut).await.is_err(),
        "停机后 submit 必须报错（不静默丢弃）"
    );
    // 此处 drop 引擎（rt/transport 已在 drain 中销毁）——不得 panic。
}

/// 控制报文 `{"__ctl":"drain","timeout_ms":N}` 的 req 形态。
fn ctl_drain_req(timeout_ms: u64) -> String {
    json!({ CTL_KEY: CTL_DRAIN, "timeout_ms": timeout_ms }).to_string()
}

/// A4：**控制报文** drain（生产停机的唯一入口）—— 插件引擎是 `OnceLock` 单例（只给
/// `&`，`drain` 取不到 `&mut`），故 `submit({"__ctl":"drain"})` 必须能触发 graceful drain：
/// 等在途 job 跑完（结果照常上送）→ 回 `{code:0,data:{drained:true}}` → 此后 submit
/// fail-loud。ctl 路**同步**完成（drain 在 `submit` 内跑完才返回 ready future）。
#[tokio::test(flavor = "multi_thread")]
async fn ctl_drain_waits_inflight_then_refuses_new_jobs() {
    let (target, gate, mut started) = gated_target(Duration::from_secs(5));
    let (sink, mut delivered) = collector();
    let eng = Arc::new(MailEngine::with_targets(targets_with(target), 2, 4, sink).expect("引擎"));

    // 在途 job：worker 已取件并卡在闸门上（enqueue 语义 ⇒ 不等结果）。
    let _ = eng.submit(PROFILE, &req_raw(Some("j-ctl"), true), vec![]);
    started.recv().await.expect("worker 应已进入投递");

    // 控制报文在 blocking 池里发（drain 是同步等待，不占 async 线程）。
    let e = Arc::clone(&eng);
    let handle = tokio::task::spawn_blocking(move || {
        let fut = e.submit("", &ctl_drain_req(5_000), vec![]);
        // ctl 路同步完成：首次 poll 即就绪（drain 已在 submit 内跑完）。
        let code = (fut.poll)(fut.state);
        assert_eq!(code, 1, "ctl drain 的 future 应即刻就绪（drain 同步完成）");
        let taken: Result<RBytes, RString> = std::result::Result::from((fut.take)(fut.state));
        (fut.free)(fut.state); // FfiFuture 无 Drop：free 由本侧显式配对
        match taken {
            Ok(b) => b.iter().copied().collect::<Vec<u8>>(),
            Err(e) => panic!("ctl drain 回了 Err: {}", &e[..]),
        }
    });

    // 放行在途 job：drain 必须等它跑完（而不是超时丢件）。
    tokio::time::sleep(Duration::from_millis(150)).await;
    gate.add_permits(1);
    let out = handle.await.expect("drain 线程不得 panic");
    let v: Value = serde_json::from_slice(&out).expect("ctl drain 回信封");
    assert_eq!(v["code"], CODE_OK, "ctl drain 信封: {v}");
    assert_eq!(v["data"]["drained"], true, "{v}");
    assert_eq!(v["data"]["workers"], 2, "{v}");

    // 在途 job 真跑完并上送了（drain 等到了，不是丢件）。
    let (topic, payload) = delivered
        .try_recv()
        .expect("在途 job 的结果应在 drain 前上送");
    assert_eq!(topic, TOPIC_MAIL_RESULT, "{payload:?}");
    // 停机后一律 fail-loud：普通投递与再次 drain 都不再入队。
    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-after-ctl"), false), vec![]);
    let e = drive(&mut fut).await.expect_err("停机后 submit 必须报错");
    assert!(e.contains("停机"), "{e}");
}

/// 控制报文解析：未知 `__ctl` fail-loud（**不**静默当业务 req）；无 `__ctl` 的 req 走业务路
/// （故「非法 JSON」与「未知 profile」的错误文案仍来自业务路）。
#[tokio::test(flavor = "multi_thread")]
async fn ctl_unknown_name_fails_loud_and_plain_req_takes_business_path() {
    let eng = MailEngine::with_targets(
        targets_with(ok_target(Duration::from_secs(1))),
        1,
        4,
        DeliverSink::new(|_, _| {}),
    )
    .expect("引擎");

    let mut fut = eng.submit(PROFILE, &json!({ CTL_KEY: "nope" }).to_string(), vec![]);
    let e = drive(&mut fut).await.expect_err("未知控制报文必须 Err");
    assert!(e.contains("nope") && e.contains(CTL_DRAIN), "{e}");

    // 无 `__ctl` ⇒ 业务路（未知 profile 的既有文案，不被控制报文分支吞掉）。
    let mut fut = eng.submit("no-such", &req_raw(Some("j-plain"), false), vec![]);
    let e = drive(&mut fut).await.expect_err("未知 profile 仍报业务错");
    assert!(e.contains("no-such"), "{e}");
    assert!(eng.drain(Duration::from_secs(2)).is_ok());
}

/// TDD-5：未知 profile 必须 fail-loud（不回落 default）。
#[tokio::test(flavor = "multi_thread")]
async fn unknown_key_returns_error() {
    let eng = MailEngine::with_targets(
        targets_with(ok_target(Duration::from_secs(1))),
        1,
        4,
        DeliverSink::new(|_, _| {}),
    )
    .expect("引擎");
    let mut fut = eng.submit("typo", &req_raw(Some("j-x"), false), vec![]);
    let e = drive(&mut fut)
        .await
        .expect_err("未知 profile 必须 Err（不回落 default）");
    assert!(e.contains("typo"), "错误须含 key，便于定位: {e}");
    // 未入队（fast-fail）⇒ 队列空，drain 立即可完成。
    assert!(eng.drain(Duration::from_secs(2)).is_ok());
}

/// TDD-5b：worker 侧的未知 key 兜底分支（submit 期已 fast-fail；直调内部函数覆盖）。
#[tokio::test(flavor = "multi_thread")]
async fn worker_reports_unknown_key_as_validation_envelope() {
    let job = Job {
        key: "nope".to_string(),
        req: req_raw(Some("j-兜底"), false),
        atts: Vec::new(),
        respond: None,
        enqueue_only: false,
        job_id: "j-兜底".to_string(),
        sync: false,
    };
    let env = deliver_one(&job, &HashMap::new()).await;
    let v: Value = serde_json::from_slice(env.as_bytes()).unwrap();
    assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
    assert_eq!(v["data"]["jobId"], "j-兜底");
}

// ---- 阶段 5：两条投递路（结构化组装 / raw 原文）----

/// 结构化组装路：text+html+附件 → `multipart/alternative`（备选体）套在 `mixed` 里，
/// 附件名/类型来自请求与宿主，**字节来自 vtable `atts` 且不被 base64 化**（ASCII 内容
/// 走 7bit）。断言读的是**真落盘的 `.eml`**，证明组装结果确实过了队列与 transport。
#[tokio::test(flavor = "multi_thread")]
async fn assemble_path_sends_multipart_with_host_resolved_attachment_bytes() {
    let dir = temp_dir("engine-assemble");
    let eng = file_engine(&dir);
    let atts = vec![att_bytes("a.pdf", "application/pdf", b"%PDF-1.4")];
    let eml = submit_and_read_eml(&eng, &req_assemble("j-asm-1", true), atts, &dir).await;

    assert!(eml.contains("multipart/alternative"), "备选体缺失: {eml}");
    assert!(
        eml.contains("multipart/mixed"),
        "有附件应有 mixed 外层: {eml}"
    );
    assert!(eml.contains("a.pdf"), "附件名缺失: {eml}");
    assert!(eml.contains("application/pdf"), "附件类型缺失: {eml}");
    assert!(
        !eml.contains("JVBERi0xLjQ"),
        "附件字节不得被 base64 化成文本流: {eml}"
    );
    assert!(
        eml.contains("hi") && eml.contains("<b>hi</b>"),
        "两版正文都在: {eml}"
    );
    assert!(
        eml.contains("To: to@example.com"),
        "信封/报头应对齐 to: {eml}"
    );
}

/// raw 路（**结构化为空**）：原文 `From`/`To` 被剥离、报头由结构化信封重建（防双收件人 /
/// 发件人 spoof），而原文 `Subject` **保留**（决策：Subject 非信封字段，剥它只会丢主题）。
/// 以**落盘 `.eml`** 为证。
#[tokio::test(flavor = "multi_thread")]
async fn raw_path_drops_envelope_headers_but_keeps_raw_subject() {
    let dir = temp_dir("engine-raw");
    let eng = file_engine(&dir);
    let req = json!({
        "from": "from@example.com",
        "to": ["to@example.com"],
        "jobId": "j-raw-1",
        "raw": "From: evil@x\nTo: victim@x\nSubject: raw-sub\nX-Keep: 1\n\nbody",
    })
    .to_string();
    let eml = submit_and_read_eml(&eng, &req, vec![], &dir).await;

    assert!(
        eml.contains("X-Keep: 1") && eml.contains("body"),
        "其余头与正文保留: {eml}"
    );
    assert!(
        !eml.contains("evil@x") && !eml.contains("victim@x"),
        "原文信封头已剥离: {eml}"
    );
    assert!(
        eml.contains("Subject: raw-sub"),
        "结构化为空时原文 Subject 必须保留: {eml}"
    );
    assert_eq!(
        eml.to_lowercase().matches("subject:").count(),
        1,
        "只能有一个 Subject 头: {eml}"
    );
    assert!(
        eml.contains("From: from@example.com") && eml.contains("To: to@example.com"),
        "报头由结构化信封重建: {eml}"
    );
}

/// raw 路（**结构化 `subject` 非空**）：以结构化值为准覆盖原文 Subject —— 落盘 `.eml` 里
/// 只有一个 Subject 头，且原文那一个（连同其值）不残留。非 ASCII 主题走 RFC2047 编码。
#[tokio::test(flavor = "multi_thread")]
async fn raw_path_structured_subject_overrides_raw_subject() {
    let dir = temp_dir("engine-raw-subject");
    let eng = file_engine(&dir);

    // 1) ASCII 结构化主题：可逐字断言（不必解 RFC2047）
    let req = json!({
        "from": "from@example.com",
        "to": ["to@example.com"],
        "jobId": "j-raw-sub-1",
        "subject": "struct-sub",
        "raw": "Subject: raw-sub\n  folded-leak\nX-Keep: 1\n\nbody",
    })
    .to_string();
    let eml = submit_and_read_eml(&eng, &req, vec![], &dir).await;
    assert!(eml.contains("Subject: struct-sub"), "结构化主题生效: {eml}");
    assert!(
        !eml.contains("raw-sub") && !eml.contains("folded-leak"),
        "原文 Subject 及其折行续行不得残留: {eml}"
    );
    assert_eq!(
        eml.to_lowercase().matches("subject:").count(),
        1,
        "最终只能有一个 Subject 头: {eml}"
    );
    assert!(eml.contains("X-Keep: 1") && eml.contains("body"), "{eml}");

    // 2) 非 ASCII 结构化主题：lettre 做 RFC2047 编码（原文 Subject 同样不残留）
    let req = json!({
        "from": "from@example.com",
        "to": ["to@example.com"],
        "jobId": "j-raw-sub-2",
        "subject": "结构主题",
        "raw": "Subject: raw-sub\n\nbody",
    })
    .to_string();
    let eml = submit_and_read_eml(&eng, &req, vec![], &dir).await;
    assert!(
        eml.contains("Subject: =?utf-8?") && !eml.contains("raw-sub"),
        "非 ASCII 主题应 RFC2047 编码且覆盖原文: {eml}"
    );
}

/// 附件下标对齐失败（请求声明 N 个、宿主解析出 M 个）必须 `code:5` fail-loud，
/// 绝不静默把没字节的附件发出去（`message.rs` 的对齐契约）。
#[tokio::test(flavor = "multi_thread")]
async fn attachment_count_mismatch_returns_code5() {
    let (sink, _delivered) = collector();
    let eng = MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 4, sink)
        .expect("引擎");
    let v = submit_expect_fail(&eng, &req_assemble("j-mismatch", true), vec![]).await;
    assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
    assert!(
        v["msg"].as_str().unwrap().contains("附件"),
        "msg 须点明附件对齐失败: {v}"
    );
    assert_eq!(v["data"]["jobId"], "j-mismatch");
    assert!(eng.drain(Duration::from_secs(5)).is_ok());
}

/// 无 `raw` 且无正文 → `code:5`（**替换**阶段 4 的「缺 raw 未实现组装」）：不许发空信。
#[tokio::test(flavor = "multi_thread")]
async fn assemble_path_without_body_returns_code5() {
    let (sink, _delivered) = collector();
    let eng = MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 4, sink)
        .expect("引擎");
    let req =
        json!({"from": "f@example.com", "to": ["t@example.com"], "jobId": "j-nobody"}).to_string();
    let v = submit_expect_fail(&eng, &req, vec![]).await;
    assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
    let msg = v["msg"].as_str().unwrap();
    assert!(
        msg.contains("text") && msg.contains("html"),
        "msg 须给下一步: {v}"
    );
    assert!(eng.drain(Duration::from_secs(5)).is_ok());
}

/// raw 路的结构化 `cc`/`bcc` 无对应报头（原文里的 Cc/Bcc 头会被剥离）→ 必须 fail-loud
/// 而不是静默丢件 / 把 Bcc 塞进 To 头；raw 与附件同样互斥（vtable 契约）。
#[tokio::test(flavor = "multi_thread")]
async fn raw_path_rejects_cc_bcc_and_attachments() {
    let (sink, _delivered) = collector();
    let eng = MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 8, sink)
        .expect("引擎");

    for extra in [
        json!({"cc": ["c@example.com"]}),
        json!({"bcc": ["b@example.com"]}),
    ] {
        let mut v = json!({
            "from": "f@example.com",
            "to": ["t@example.com"],
            "raw": "X-Keep: 1\n\nbody",
            "jobId": "j-raw-cc",
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        let env = submit_expect_fail(&eng, &v.to_string(), vec![]).await;
        assert_eq!(env["code"], CODE_VALIDATION, "信封: {env}");
        assert!(
            env["msg"].as_str().unwrap().contains("cc/bcc"),
            "信封: {env}"
        );
    }

    // raw + 声明附件（宿主按契约不解析 → 走 refs 校验）与 raw + 已解析字节，两者都拒。
    let mut v = json!({
        "from": "f@example.com",
        "to": ["t@example.com"],
        "raw": "X-Keep: 1\n\nbody",
        "jobId": "j-raw-att",
        "attachments": [{"filename": "a.pdf", "blobKey": "k"}],
    });
    let env = submit_expect_fail(&eng, &v.to_string(), vec![]).await;
    assert!(env["msg"].as_str().unwrap().contains("附件"), "信封: {env}");

    v["attachments"] = json!([]);
    let env = submit_expect_fail(
        &eng,
        &v.to_string(),
        vec![att_bytes("a.pdf", "application/pdf", b"x")],
    )
    .await;
    assert!(env["msg"].as_str().unwrap().contains("附件"), "信封: {env}");

    assert!(eng.drain(Duration::from_secs(5)).is_ok());
}

/// 真 SMTP 路（不连网即失败）：端口 1 无监听 → `code:1`（网络/连接类），
/// 且错误文案**不含** SMTP 对话/凭据。
#[tokio::test(flavor = "multi_thread")]
async fn smtp_connection_refused_returns_network_code() {
    let cfg = MailConfig::parse(
            r#"{"workers":2,"queue_capacity":4,"default":{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"login","timeout":5}}"#,
        )
        .expect("cfg");
    let eng =
        MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎（建 transport 不连网）");
    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-refused"), false), vec![]);
    let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
    assert_eq!(v["code"], CODE_NETWORK, "信封: {v}");
    assert_eq!(v["data"]["jobId"], "j-refused");
    assert_eq!(v["msg"], "投递失败", "msg 须为脱敏分类文案: {v}");
}

/// design §10「每 job `tokio::time::timeout(profile.timeout)`；超时 `code:1`」——**异步路**：
/// 投递挂住（闸门永不放行）→ future 在超时点 resolve `code:1`（既不无限等待，也不 panic
/// 整个 worker），msg 为脱敏分类文案。
#[tokio::test(flavor = "multi_thread")]
async fn async_delivery_timeout_returns_network_code() {
    let (target, _gate, mut started) = gated_target(Duration::from_millis(50));
    let eng = MailEngine::with_targets(targets_with(target), 1, 4, DeliverSink::new(|_, _| {}))
        .expect("引擎");
    let mut fut = eng.submit(PROFILE, &req_raw(Some("j-timeout"), false), vec![]);
    // 先确认 worker **已进入投递**（否则下面测到的可能只是「还没开始」而非超时）。
    started.recv().await.expect("worker 应已进入投递");

    let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("超时必须回信封")).unwrap();
    assert_eq!(v["code"], CODE_NETWORK, "超时归连接/网络类: {v}");
    assert_eq!(v["data"]["jobId"], "j-timeout");
    assert_eq!(
        v["msg"], "投递超时",
        "msg 须为脱敏分类文案（无 SMTP 对话）: {v}"
    );
    assert!(
        eng.drain(Duration::from_secs(5)).is_ok(),
        "超时后 worker 应能正常 drain"
    );
}

/// 同步路同款（`deliver_one` 的 `spawn_blocking` 分支）：阻塞投递超过 `profile.timeout`
/// → 同样 `code:1`。`timeout` 只停止**等待**，池中已开始的阻塞调用会跑完（诚实边界，
/// 见 `deliver_one` 注释）——本用例末尾放行闸门，不留挂死的 blocking 线程。
#[tokio::test(flavor = "multi_thread")]
async fn sync_delivery_timeout_returns_network_code() {
    // 阻塞闸门：`Mutex<Receiver>` 同时满足 Send + Sync（`Receiver` 只 Send）。
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let blocked = Mutex::new(blocked);
    let target = MailTarget::new(
        Duration::from_millis(50),
        // 本用例只走同步路（req.sync = true），异步路给立即成功的桩。
        Arc::new(|_, _| Box::pin(async { Ok("unused".to_string()) })),
        Arc::new(move |_, _| {
            let _ = blocked.lock().expect("闸门锁").recv();
            Err("blocked send".to_string())
        }),
    );
    let eng = MailEngine::with_targets(targets_with(target), 1, 4, DeliverSink::new(|_, _| {}))
        .expect("引擎");
    let req = json!({
        "from": "from@example.com", "to": ["to@example.com"],
        "raw": "Subject: t\r\n\r\nbody", "sync": true, "jobId": "j-sync-timeout",
    })
    .to_string();

    let mut fut = eng.submit(PROFILE, &req, vec![]);
    let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("超时必须回信封")).unwrap();
    assert_eq!(v["code"], CODE_NETWORK, "同步路超时归连接/网络类: {v}");
    assert_eq!(v["data"]["jobId"], "j-sync-timeout");
    assert_eq!(v["msg"], "投递超时", "{v}");

    let _ = release.send(()); // 放行阻塞线程
    assert!(eng.drain(Duration::from_secs(5)).is_ok());
}

/// design §10/§11「`msg` 脱敏（无账号/密码/令牌/SMTP 对话）」+「令牌不进信封」：
/// 带凭据的 profile 投递失败 → 信封里**不得**出现口令、令牌或用户名，且信封保持
/// **严格白名单形态**（`{code,msg,data:{jobId}}`，msg 为分类文案、无原始诊断细节）。
/// 凭据只在插件 cfg 里（不进 req），故信封一旦带凭据/对话即是回归。
#[tokio::test(flavor = "multi_thread")]
async fn failure_envelope_never_carries_credentials() {
    const PASS: &str = "S3CRET-PASS-42";
    const TOKEN: &str = "ya29.S3CRET-TOKEN-42";
    const USER: &str = "mailer@example.com";
    // 端口 1 无监听 → 真连接失败（凭据确实参与过认证路径，只是尚未发出即断）。
    let cfg = MailConfig::parse(&format!(
            r#"{{"workers":2,"queue_capacity":4,
                 "login":{{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"login","user":"{USER}","pass":"{PASS}","timeout":5}},
                 "oauth":{{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"xoauth2","user":"{USER}","xoauth2":{{"access_token":"{TOKEN}"}},"timeout":5}}}}"#
        ))
        .expect("cfg");
    let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎");

    for (profile, job) in [("login", "j-login"), ("oauth", "j-oauth")] {
        let mut fut = eng.submit(profile, &req_raw(Some(job), false), vec![]);
        let out = drive(&mut fut).await.expect("应回信封");
        let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
        assert_eq!(v["code"], CODE_NETWORK, "{profile}: {v}");
        // 严格形态：只有 code/msg/data 三个键，data 只有 jobId —— 不得夹带诊断细节。
        // （键序无关：serde_json 的 Map 后端可能是 BTreeMap，故排序后比对。）
        let mut top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        top.sort_unstable();
        assert_eq!(top, ["code", "data", "msg"], "{profile}: {v}");
        let data: Vec<&str> = v["data"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(data, ["jobId"], "{profile}: {v}");
        assert_eq!(v["msg"], "投递失败", "{profile} 的 msg 须为分类文案: {v}");

        let text = String::from_utf8(out).expect("UTF-8");
        for leaked in [PASS, TOKEN, USER, "AUTH", "LOGIN"] {
            assert!(
                !text.contains(leaked),
                "{profile} 的信封泄露凭据/认证对话（{leaked}）: {text}"
            );
        }
    }
}

/// 生产路径构造「SMTP（pool）transport」引擎：建 transport 本身**不连网**（首次 send
/// 才连），但该 transport 的 Drop 会 `tokio::spawn`（lettre `pool/async_impl.rs:262`）。
///
/// 注意：`build_profiles` **必须**在 runtime 上下文内调用（`MailEngine::new` 已 `rt.enter()`）
/// ——阶段 4 实测：在无 runtime 上下文的同步线程上构建，构建期的临时 Pool 析构就会
/// panic（"there is no reactor running"）。
fn pool_engine(workers: usize) -> MailEngine {
    MailEngine::new(&pool_cfg(workers), DeliverSink::new(|_, _| {})).expect("引擎")
}

/// 单 profile、明文、指向本地无监听端口的配置（`tls:none` 需显式允许）。
fn pool_cfg(workers: usize) -> MailConfig {
    MailConfig::parse(&format!(
            r#"{{"workers":{workers},"queue_capacity":4,"default":{{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"login","timeout":1}}}}"#
        ))
        .expect("cfg")
}

/// 硬约束（模块头 §2）证据 · Drop 路：在 async 上下文（`#[tokio::test]`、宿主 async 任务）
/// 里 drop 引擎必须安全（`dispose` 用非阻塞的 `shutdown_background`）。
///
/// 变异验证：把 `dispose` 的 `rt.shutdown_background()` 换成裸 `drop(rt)` → 本用例 panic
/// （tokio `blocking/shutdown.rs:51` "Cannot drop a runtime in a context where blocking is
/// not allowed"）。
#[tokio::test(flavor = "multi_thread")]
async fn dropping_engine_in_async_context_does_not_abort() {
    // 同时覆盖注入路（`build_profiles` + `into_target` + `with_targets`）：它在 async
    // 上下文里调用，故顺带证明 `build_profiles` 的「必须在 runtime 上下文内」约束。
    let mut targets = HashMap::new();
    for (k, p) in build_profiles(&pool_cfg(2)).expect("build").into_iter() {
        targets.insert(k, p.into_target());
    }
    let eng = MailEngine::with_targets(targets, 2, 4, DeliverSink::new(|_, _| {})).expect("引擎");
    // 不投递（无网络）；只验证销毁路径在 async 上下文里不 panic。
    drop(eng);
}

/// 硬约束（模块头 §1）证据 · drain 路：`drain` 后各 worker 已退出并释放自己的强引用，
/// 于是 transport 的**最后一份强引用**在 `dispose` 里释放 —— 必须在 rt 上下文内
/// （lettre `pool` 的 Drop 会 `tokio::spawn`）。
///
/// 变异验证：去掉 `dispose` 的 `rt.enter()` → 本用例 panic（"there is no reactor running"）。
#[tokio::test(flavor = "multi_thread")]
async fn drain_releases_pool_transports_inside_runtime_context() {
    let eng = pool_engine(2);
    eng.drain(Duration::from_secs(5))
        .expect("空队列的 drain 应立即成功（worker 全部退出）");
    drop(eng); // rt/targets 已在 drain 内销毁；此处不得 panic
}

/// 同一约束的**决定性**版本：同步上下文（进程退出 / 宿主非 async 路径）里线程上
/// **没有任何** runtime，`rt.enter()` 是唯一让 lettre pool 的 Drop 能 `tokio::spawn`
/// 的东西。（上一条异步用例里测试自身 runtime 会「兜住」漏掉的 enter，故单靠它无法
/// 分辨 —— 变异验证：去掉 `dispose` 里的 `rt.enter()`，本用例 panic
/// "there is no reactor running"，异步那条仍绿。）
#[test]
fn drain_from_sync_context_releases_pool_transports_in_rt_context() {
    let eng = pool_engine(2);
    eng.drain(Duration::from_secs(5))
        .expect("空队列的 drain 应立即成功（worker 全部退出）");
}

/// A3：**在途 job** + **真 pool transport**（非注入桩）+ 两条「丢弃在途」的销毁路径
/// （非 graceful `Drop` 与 drain 超时→`dispose`）都必须无 panic。
///
/// 真 pool transport 指向「只 accept、永不应答」的本地监听：SMTP 握手挂在投递中，
/// job 跑不完 ⇒ worker 帧（连同它捕获的 `targets` 强引用与在途投递 future）只能在
/// runtime 停机时被丢弃。lettre `pool` 的 Drop 会 `tokio::spawn`（`pool/async_impl.rs:262`），
/// 故「最后一份强引用的销毁点是否有 runtime 上下文」是这条路径的成败关键。
///
/// **实测结论（本用例在当前依赖下是绿的，非「先红后绿」）**：tokio 1.53 在
/// `shutdown_background` 取消未完成任务时会先进入 runtime 上下文
/// （`OwnedTasks::shutdown` 走 `try_enter_blocking_region`，实测 `Handle::try_current()`
/// 为 `Ok`、`tokio::spawn` 成功），故**没有**出现「无上下文析构 → panic」。
/// 但这属于 tokio 的实现细节：worker 帧的释放点若晚于退出信号，正确性就不再由本模块
/// 的顺序保证所决定（见 `with_rt` 的「顺序即契约」注释）——本用例因此是**回归护栏**
/// （钉住「两条丢弃路径都不得 panic」），而非该 bug 的判定证据。
#[test]
fn discarding_inflight_pool_job_does_not_panic() {
    // 真监听：accept 后只持有连接、不读不写 ⇒ SMTP 问候永不到来 ⇒ 投递挂住。
    fn hanging_listener() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for c in listener.incoming().flatten() {
                held.push(c);
            }
        });
        port
    }
    fn engine_to(port: u16) -> MailEngine {
        let cfg = MailConfig::parse(&format!(
                r#"{{"workers":1,"queue_capacity":4,"default":{{"host":"127.0.0.1","port":{port},"tls":"none","allow_none_tls":true,"mechanism":"login","timeout":30}}}}"#
            ))
            .expect("cfg");
        MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎")
    }

    // (1) 非 graceful Drop：不 drain，直接丢弃（在途 job 随 runtime 一起丢）。
    let eng = engine_to(hanging_listener());
    let _ = eng.submit(PROFILE, &req_raw(Some("j-drop"), true), vec![]);
    std::thread::sleep(Duration::from_millis(500));
    drop(eng);

    // (2) drain 超时：在途 job 挂住 ⇒ drain 到点返回 Err，随后 dispose（同样丢在途）。
    let eng = engine_to(hanging_listener());
    let _ = eng.submit(PROFILE, &req_raw(Some("j-drain-timeout"), true), vec![]);
    std::thread::sleep(Duration::from_millis(500));
    let e = eng
        .drain(Duration::from_millis(200))
        .expect_err("在途 job 挂住 ⇒ drain 必须超时报错");
    assert!(e.contains("超时"), "{e}");
    drop(eng); // rt/targets 已在 drain 内销毁；此处不得 panic
}
