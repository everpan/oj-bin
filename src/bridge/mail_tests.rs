use super::*;
use crate::bridge::{
    Bridge, Bus, EventBroker, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry, WsSend,
};
use oj_plugin_ffi::MailVtable;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

/// 全局 deliver 槽是进程级的：本模块用例串行化（同 `ffi.rs::adapter_tests` 手法）。
static T_LOCK: Mutex<()> = Mutex::new(());

/// 取共享锁：用例 panic 后锁被毒化，后续用例取 `into_inner()` 继续（不连锁失败）。
fn lock() -> std::sync::MutexGuard<'static, ()> {
    T_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- 假后端（无插件依赖） ----------

struct FakeMail {
    config: MailConfig,
    router: MailResultRouter,
    sent: Mutex<Vec<(String, String, Vec<ParsedAttachment>)>>,
}

impl FakeMail {
    fn new(config: MailConfig, bus: Arc<dyn EventBroker>) -> Arc<Self> {
        Arc::new(Self {
            config,
            router: MailResultRouter::new(bus),
            sent: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl MailBackend for FakeMail {
    async fn submit(
        &self,
        key: &str,
        req: String,
        atts: Vec<ParsedAttachment>,
    ) -> crate::bridge::BridgeResult<Value> {
        self.sent.lock().unwrap().push((key.to_string(), req, atts));
        Ok(json!({"code": 0, "msg": "ok", "data": {"jobId": "j-stub"}}))
    }
    fn config(&self) -> &MailConfig {
        &self.config
    }
    fn router(&self) -> &MailResultRouter {
        &self.router
    }
}

#[test]
fn given_installed_backend_when_query_then_installed_true() {
    let _g = lock();
    let b: Arc<dyn MailBackend> = FakeMail::new(MailConfig::empty(), Arc::new(Bus::new()));
    install_mail_deliver(&b);
    assert!(mail_deliver_installed());
}

#[test]
fn given_dropped_backend_when_query_then_installed_false() {
    let _g = lock();
    {
        let b: Arc<dyn MailBackend> = FakeMail::new(MailConfig::empty(), Arc::new(Bus::new()));
        install_mail_deliver(&b);
    } // 弱引用随强引用消亡
    assert!(!mail_deliver_installed());
}

/// 假插件上送的统一信封（`HostContext.deliver(MAIL_RESULT_TOPIC, …)` 的载荷）。
/// 故意夹带 `to`/`subject`：扁平化必须按白名单剔除（design §5/§11 脱敏）。
const ENVELOPE: &[u8] = br#"{"code":0,"msg":"sent","data":{"jobId":"j1","messageId":"m1","to":["a@x.com"],"subject":"s"}}"#;

fn deliver_to_host(topic: &str, payload: &[u8]) {
    crate::bridge::ffi::host_deliver(
        oj_plugin_ffi::RString::from(topic),
        oj_plugin_ffi::RBytes::from(payload),
    );
}

// ---------- 6.2：StableState/Extras 注入 ----------

/// 测试用归属：profile = "default"，无模块/租户上下文（`CallCtx::default()`）。
fn owner() -> MailOwner {
    MailOwner::new("default", &CallCtx::default())
}

/// Extras.mail 注入 → StableState.mail 取用 → 插件上送的信封落进结果存储
/// （deliver 路由已由构造期挂上）。
///
/// B3：上送只能**填充宿主已登记的票**（`reserve` 由 `handle_send` 在 submit 前做），
/// 故这里先登记再上送。
#[tokio::test(flavor = "current_thread")]
async fn bridge_injects_mail_backend_and_routes_deliver() {
    let _g = lock();
    let bus = Arc::new(Bus::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    bus.subscribe(MAIL_RESULT_TOPIC, tx);
    let fake = FakeMail::new(MailConfig::empty(), bus.clone());
    let _b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            mail: Some(fake.clone()),
            ..Default::default()
        },
    );
    assert!(fake.router().reserve("j1", owner()), "宿主先开票");
    // 插件 worker 上送 → 宿主路由：存结果 + 本地扇出。
    deliver_to_host(MAIL_RESULT_TOPIC, ENVELOPE);
    let stored = fake.router().get("j1", &owner()).expect("结果必须已存");
    assert_eq!(stored["code"], 0);
    assert_eq!(stored["messageId"], "m1");
    // 扇出帧：扁平形态，且**不含** to/subject。
    let WsSend::Text(frame) = rx.try_recv().expect("订阅者必须收到扇出") else {
        panic!("mail.result 扇出应为文本帧");
    };
    let v: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(v["topic"], MAIL_RESULT_TOPIC);
    assert_eq!(v["data"]["jobId"], "j1");
    assert_eq!(v["data"]["code"], 0);
    assert_eq!(v["data"]["messageId"], "m1");
    assert!(v["data"].get("to").is_none(), "{v}");
    assert!(v["data"].get("subject").is_none(), "{v}");
    assert_eq!(v["data"].as_object().unwrap().len(), 4, "{v}");
}

/// 未配置（StableState.mail = None）→ 路由明确报「未配置」（与「载荷非法」分开，
/// 告警文案据此区分）；不 panic（结果丢弃）。
#[tokio::test(flavor = "current_thread")]
async fn route_deliver_without_backend_is_not_configured() {
    let _g = lock();
    let _b = Bridge::new(
        Arc::new(InMemoryAccessor::new()),
        Arc::new(InMemoryKV::new()),
    );
    assert!(matches!(
        route_deliver(ENVELOPE),
        DeliverRoute::NotConfigured
    ));
}

/// A6 + B3：上送的**四种成因分别归类**（告警文案据此区分，不混为一句）：
/// `BadPayload` = 载荷不是结果信封（非 JSON / 缺 `jobId`/`code`/`msg`）；
/// `UnknownJob` = jobId 不是本宿主开出的票（未登记）；`Duplicate` = 已有结果（拒绝覆盖）；
/// 其余 = `Routed`。
#[tokio::test(flavor = "current_thread")]
async fn route_deliver_distinguishes_bad_payload_unknown_and_duplicate() {
    let _g = lock();
    let bus = Arc::new(Bus::new());
    let fake = FakeMail::new(MailConfig::empty(), bus.clone());
    let _b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            mail: Some(fake.clone()),
            ..Default::default()
        },
    );
    for bad in [
        &b"not json"[..],
        &br#"{"code":0,"msg":"ok","data":{}}"#[..], // 缺 jobId
        &br#"{"data":{"jobId":"j"}}"#[..],          // 缺 code/msg
    ] {
        assert!(
            matches!(route_deliver(bad), DeliverRoute::BadPayload),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    // 合法信封 → Routed（对照：不是所有载荷都被判非法）——但 B3 起**先要宿主开票**。
    assert!(matches!(route_deliver(ENVELOPE), DeliverRoute::UnknownJob));
    assert!(
        fake.router().get("j1", &owner()).is_none(),
        "未开票的 jobId 不得写入结果"
    );
    assert!(fake.router().reserve("j1", owner()));
    assert!(matches!(route_deliver(ENVELOPE), DeliverRoute::Routed));
    assert!(fake.router().get("j1", &owner()).is_some());
    // B3：同一 jobId 二次上送 → 拒绝覆盖（不扇出、不改存储）。
    assert!(matches!(route_deliver(ENVELOPE), DeliverRoute::Duplicate));
}

// ---------- 6.3：结果存储 + 扁平化 ----------

#[test]
fn flatten_result_whitelists_fields_and_needs_job_id() {
    let e: Value = serde_json::from_slice(ENVELOPE).unwrap();
    assert_eq!(
        flatten_result(&e).unwrap(),
        json!({"jobId": "j1", "code": 0, "msg": "sent", "messageId": "m1"})
    );
    // 无 jobId → 无法索引，丢弃（不 panic）。
    assert!(flatten_result(&json!({"code": 0, "msg": "ok", "data": {}})).is_none());
    // 无 code/msg（非信封形态）→ 丢弃。
    assert!(flatten_result(&json!({"data": {"jobId": "j"}})).is_none());
}

/// 限长 + TTL（登记与完成都算条目；超限淘汰最旧、过期惰性清理）。
#[test]
fn result_store_caps_oldest_and_expires_by_ttl() {
    let s = MailResultStore::new(2, Duration::from_millis(60));
    let owner = MailOwner::new("default", &CallCtx::default());
    for id in ["a", "b", "c"] {
        assert!(s.reserve(id, owner.clone()));
        assert_eq!(
            s.complete(id, json!({"jobId": id})),
            CompleteOutcome::Filled
        );
    }
    assert!(s.get("a", &owner).is_none(), "超限须淘汰最旧");
    assert!(s.get("b", &owner).is_some());
    assert_eq!(s.get("c", &owner).unwrap()["jobId"], "c");
    assert_eq!(s.len(), 2, "限长：容量恒定");
    std::thread::sleep(Duration::from_millis(80));
    assert!(s.get("c", &owner).is_none(), "TTL 过期后不可读");
    assert!(s.is_empty(), "过期项被惰性清理");
}

/// B3：同 jobId **不得覆写**（原实现 `put` 同 id 覆盖 → 任意模块可把他人结果改写成
/// `code:0`）。宿主开出的票只能被**填充一次**；未登记的 jobId 不得凭空写入结果。
#[test]
fn result_store_refuses_to_overwrite_same_job_id() {
    let s = MailResultStore::new(4, Duration::from_secs(60));
    let owner = MailOwner::new("default", &CallCtx::default());
    assert!(s.reserve("j", owner.clone()), "首次登记成功");
    assert!(
        !s.reserve("j", owner.clone()),
        "同 jobId 不得重复登记（不覆盖）"
    );
    assert_eq!(
        s.complete("j", json!({"jobId": "j", "code": 0})),
        CompleteOutcome::Filled
    );
    assert_eq!(
        s.complete("j", json!({"jobId": "j", "code": 2, "msg": "forged"})),
        CompleteOutcome::Duplicate,
        "已有结果必须拒绝覆盖"
    );
    assert_eq!(s.len(), 1, "拒绝覆盖不占额外容量");
    assert_eq!(
        s.get("j", &owner).unwrap()["code"],
        0,
        "先到的结果必须保持（覆写 = 结果通道可被伪造）"
    );
    // 未登记的 jobId（伪造的票）不得写入结果。
    assert_eq!(
        s.complete("ghost", json!({"jobId": "ghost", "code": 0})),
        CompleteOutcome::UnknownJob
    );
    assert!(s.get("ghost", &owner).is_none());
}

/// B3：读取按**归属**（profile + 模块 + 租户）隔离 —— 三者任一不同都视作「未命中」，
/// 不泄露该 jobId 是否存在（跨模块 / 跨租户枚举面）。
#[test]
fn result_store_scopes_results_to_owner() {
    let s = MailResultStore::new(4, Duration::from_secs(60));
    let me = MailOwner {
        profile: "default".into(),
        module: "user".into(),
        tenant: "t1".into(),
    };
    assert!(s.reserve("j", me.clone()));
    assert_eq!(
        s.complete("j", json!({"jobId": "j", "code": 0})),
        CompleteOutcome::Filled
    );
    assert!(s.get("j", &me).is_some(), "本人必须读得到");
    for alien in [
        MailOwner {
            module: "other".into(),
            ..me.clone()
        },
        MailOwner {
            profile: "alerts".into(),
            ..me.clone()
        },
        MailOwner {
            tenant: "t2".into(),
            ..me.clone()
        },
    ] {
        assert!(
            s.get("j", &alien).is_none(),
            "非本归属不得读到（{alien:?}）"
        );
    }
}

// ---------- 6.2：vtable 适配器 ----------

/// 假 vtable 用的 **ready** FfiFuture 状态（poll/take/free 三指针，与 `ffi.rs`
/// 适配器测试同款）；`FREED` 记录 free 次数（句柄必须释放）。
/// `FREED` 是进程级静态：用它做断言的用例须持 [`lock`]（串行化，取增量）。
struct ReadyState(Option<Result<Vec<u8>, String>>);
static FREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C" fn ready_poll(state: *mut std::ffi::c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut ReadyState) };
    match &s.0 {
        Some(Ok(_)) => 1,
        Some(Err(_)) => -1,
        None => 0,
    }
}

extern "C" fn ready_take(
    state: *mut std::ffi::c_void,
) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
    let s = unsafe { &mut *(state as *mut ReadyState) };
    match s.0.take() {
        Some(Ok(b)) => oj_plugin_ffi::RResult::Ok(oj_plugin_ffi::RBytes::from(&b[..])),
        Some(Err(e)) => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from(e.as_str())),
        None => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready")),
    }
}

extern "C" fn ready_free(state: *mut std::ffi::c_void) {
    if !state.is_null() {
        FREED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(unsafe { Box::from_raw(state as *mut ReadyState) });
    }
}

fn ready_future(r: Result<Vec<u8>, String>) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::FfiFuture {
        state: Box::into_raw(Box::new(ReadyState(Some(r)))).cast(),
        poll: ready_poll,
        take: ready_take,
        free: ready_free,
    }
}

/// 单方法假 vtable 的返回体（用例串行「设置 → 立即调用」，无并发窗口）。
static SUBMIT_BODY: Mutex<Vec<u8>> = Mutex::new(Vec::new());

extern "C" fn body_submit(
    _key: oj_plugin_ffi::RString,
    _req: oj_plugin_ffi::RString,
    _atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
) -> oj_plugin_ffi::FfiFuture {
    ready_future(Ok(SUBMIT_BODY.lock().unwrap().clone()))
}

/// 构造「`submit` 恒返回 `body`」的假 vtable。
fn vtable_returning(body: &[u8]) -> &'static MailVtable {
    *SUBMIT_BODY.lock().unwrap() = body.to_vec();
    Box::leak(Box::new(MailVtable {
        submit: body_submit,
    }))
}

/// 记录 fake `submit` 收到的 `(key, req)`（A4 控制报文用）。
static SEEN_SUBMIT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

extern "C" fn record_submit(
    key: oj_plugin_ffi::RString,
    req: oj_plugin_ffi::RString,
    _atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
) -> oj_plugin_ffi::FfiFuture {
    SEEN_SUBMIT
        .lock()
        .unwrap()
        .push((key[..].to_string(), req[..].to_string()));
    ready_future(Ok(SUBMIT_BODY.lock().unwrap().clone()))
}

/// A4：`FfiMailBackend::drain` 必须发**控制报文** `{"__ctl":"drain","timeout_ms":N}`
/// （key 为空 —— 控制报文不占 profile），并把插件的排空信封解回。
#[tokio::test(flavor = "current_thread")]
async fn ffi_mail_backend_drain_sends_control_message_and_decodes_envelope() {
    let _g = lock(); // 与用 SUBMIT_BODY/SEEN_SUBMIT 的用例串行
    let vt: &'static MailVtable = Box::leak(Box::new(MailVtable {
        submit: record_submit,
    }));
    *SUBMIT_BODY.lock().unwrap() =
        br#"{"code":0,"msg":"ok","data":{"drained":true,"workers":2}}"#.to_vec();
    SEEN_SUBMIT.lock().unwrap().clear();

    let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
    let env = b.drain(Duration::from_millis(2500)).expect("drain 信封");
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(env["data"]["drained"], true, "{env}");
    assert_eq!(env["data"]["workers"], 2, "{env}");

    let seen = SEEN_SUBMIT.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "控制报文只发一次");
    assert_eq!(seen[0].0, "", "控制报文不占 profile key");
    let req: Value = serde_json::from_str(&seen[0].1).unwrap();
    assert_eq!(req[CONTROL_KEY], CONTROL_DRAIN, "{req}");
    assert_eq!(req["timeout_ms"], 2500, "超时必须过线（插件据此等）：{req}");
}

/// A4 契约边界：插件回 **pending**（违反「控制报文同步完成」契约）或坏 JSON → 一律
/// fail-loud 报错，**绝不挂死**（drain 只有一次 poll，不做异步等待）。
#[tokio::test(flavor = "current_thread")]
async fn ffi_mail_backend_drain_fails_loud_on_pending_or_garbage() {
    let _g = lock();
    extern "C" fn pending_poll(_s: *mut std::ffi::c_void) -> i32 {
        0
    }
    extern "C" fn pending_take(
        _s: *mut std::ffi::c_void,
    ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
        oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready"))
    }
    extern "C" fn pending_free(_s: *mut std::ffi::c_void) {}
    extern "C" fn pending_submit(
        _k: oj_plugin_ffi::RString,
        _r: oj_plugin_ffi::RString,
        _a: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
    ) -> oj_plugin_ffi::FfiFuture {
        oj_plugin_ffi::FfiFuture {
            state: std::ptr::null_mut(),
            poll: pending_poll,
            take: pending_take,
            free: pending_free,
        }
    }
    let vt: &'static MailVtable = Box::leak(Box::new(MailVtable {
        submit: pending_submit,
    }));
    let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
    let e = b.drain(Duration::from_millis(10)).unwrap_err().to_string();
    assert!(e.contains("pending"), "pending 必须点名契约违约：{e}");

    // 坏 JSON（非 UTF-8/非 JSON）→ decode 错误臂。
    let vt = vtable_returning(b"{not json");
    let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
    let e = b.drain(Duration::from_millis(10)).unwrap_err().to_string();
    assert!(e.contains("ffi mail drain decode"), "{e}");
}

/// 适配器把宿主办的 `Vec<ParsedAttachment>` 按**下标原序**填进 `RVec<MailAttachment>`
/// （插件按 index 对齐，数量/顺序错位即 code:5），并把 future 结果（信封 JSON）解回。
#[tokio::test(flavor = "current_thread")]
async fn ffi_mail_backend_forwards_key_and_ordered_attachments() {
    use std::sync::atomic::Ordering;
    let _g = lock(); // FREED 是进程级静态：本用例取增量，须与用它的用例串行
    /// 假 vtable 记录的一次调用：key / req / 附件（filename, mime, bytes）。
    type Seen = (String, String, Vec<(String, String, Vec<u8>)>);
    static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());
    let freed_before = FREED.load(Ordering::SeqCst);

    extern "C" fn fake_submit(
        key: oj_plugin_ffi::RString,
        req: oj_plugin_ffi::RString,
        atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
    ) -> oj_plugin_ffi::FfiFuture {
        let mut got = Vec::new();
        for a in atts {
            got.push((
                a.filename[..].to_string(),
                a.mime[..].to_string(),
                a.bytes[..].to_vec(),
            ));
        }
        SEEN.lock()
            .unwrap()
            .push((key[..].to_string(), req[..].to_string(), got));
        ready_future(Ok(
            br#"{"code":0,"msg":"sent","data":{"jobId":"j9","messageId":"m9"}}"#.to_vec(),
        ))
    }

    let vt: &'static MailVtable = Box::leak(Box::new(MailVtable {
        submit: fake_submit,
    }));
    let bus: Arc<dyn EventBroker> = Arc::new(Bus::new());
    let b = FfiMailBackend::new(vt, MailConfig::empty(), bus);
    let atts = vec![
        ParsedAttachment {
            filename: "a.pdf".into(),
            mime: "application/pdf".into(),
            bytes: vec![1, 2, 3],
        },
        ParsedAttachment {
            filename: "b.bin".into(),
            mime: "application/octet-stream".into(),
            bytes: vec![0, 159, 255],
        },
    ];
    let env = b
        .submit("default", r#"{"subject":"s"}"#.to_string(), atts)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["jobId"], "j9");
    let seen = SEEN.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "default");
    assert_eq!(seen[0].1, r#"{"subject":"s"}"#);
    // 顺序与下标严格一致（含非 UTF-8 字节原样过线）。
    assert_eq!(
        seen[0].2,
        vec![
            (
                "a.pdf".to_string(),
                "application/pdf".to_string(),
                vec![1, 2, 3]
            ),
            (
                "b.bin".to_string(),
                "application/octet-stream".to_string(),
                vec![0, 159, 255]
            ),
        ]
    );
    assert_eq!(
        FREED.load(Ordering::SeqCst),
        freed_before + 1,
        "future 句柄须 free（本用例增量 1）"
    );
}

/// A1 **纵深防御**：插件回**裸**载荷（旧版/第三方形态，如 `{"jobId":"j-bare"}`）时，
/// 宿主必须包成统一信封 —— JS 侧 `res.data.jobId` 契约不因插件形态差异而 TypeError，
/// 且 `mail.result(jobId)` 拿得到 id。
#[tokio::test(flavor = "current_thread")]
async fn ffi_mail_backend_wraps_bare_plugin_payload_into_envelope() {
    let _g = lock(); // 与用 FREED 的用例串行（同一 ready future 脚手架）
    let cases: [(&'static [u8], &str); 3] = [
        // 历史上的裸 `{jobId}`（A1 修复前的插件形态）。
        (br#"{"jobId":"j-bare"}"#, "j-bare"),
        // 裸标量/数组同样不崩（data 原样收纳）。
        (br#""naked""#, "naked"),
        (br#"[1,2]"#, "arr"),
    ];
    for (body, tag) in cases {
        let vt = vtable_returning(body);
        let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
        let env = b
            .submit("default", "{}".to_string(), vec![])
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(env["code"], 0, "{tag}: 裸载荷须包成成功信封: {env}");
        assert_eq!(env["msg"], "ok", "{tag}: {env}");
        match tag {
            "j-bare" => assert_eq!(env["data"]["jobId"], "j-bare", "{env}"),
            "naked" => assert_eq!(env["data"], "naked", "{env}"),
            _ => assert_eq!(env["data"], json!([1, 2]), "{env}"),
        }
    }
    // 已是信封的形态**原样透传**（不重复包裹）。
    let vt = vtable_returning(br#"{"code":5,"msg":"boom","data":{}}"#);
    let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
    let env = b
        .submit("default", "{}".to_string(), vec![])
        .await
        .expect("信封");
    assert_eq!(
        env,
        json!({"code": 5, "msg": "boom", "data": {}}),
        "已有 code 的信封不得被二次包裹"
    );
}

/// A2：插件 future 长期 pending 时，宿主**退避睡眠**而非 `yield_now` 空转。
/// 20 次 pending × `FFI_POLL_BACKOFF`(2ms) ⇒ 墙钟 ≥ 30ms；空转实现只需 µs 级
/// （原实现用 `await_ffi`：SMTP 往返可达 30s ⇒ 每秒烧满该 isolate 的 current_thread）。
#[tokio::test(flavor = "current_thread")]
async fn ffi_mail_backend_backs_off_instead_of_spinning_while_pending() {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 前 `left` 次 poll 返回 pending，之后 ready。
    struct Countdown {
        left: AtomicU32,
        result: Option<Result<Vec<u8>, String>>,
    }
    extern "C" fn poll(state: *mut std::ffi::c_void) -> i32 {
        let s = unsafe { &mut *(state as *mut Countdown) };
        if s.left.load(Ordering::SeqCst) == 0 {
            1
        } else {
            s.left.fetch_sub(1, Ordering::SeqCst);
            0
        }
    }
    extern "C" fn take(
        state: *mut std::ffi::c_void,
    ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
        let s = unsafe { &mut *(state as *mut Countdown) };
        match s.result.take() {
            Some(Ok(b)) => oj_plugin_ffi::RResult::Ok(oj_plugin_ffi::RBytes::from(&b[..])),
            _ => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready")),
        }
    }
    extern "C" fn free(state: *mut std::ffi::c_void) {
        if !state.is_null() {
            drop(unsafe { Box::from_raw(state as *mut Countdown) });
        }
    }
    extern "C" fn submit(
        _key: oj_plugin_ffi::RString,
        _req: oj_plugin_ffi::RString,
        _atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
    ) -> oj_plugin_ffi::FfiFuture {
        const PENDING: u32 = 20;
        oj_plugin_ffi::FfiFuture {
            state: Box::into_raw(Box::new(Countdown {
                left: AtomicU32::new(PENDING),
                result: Some(Ok(
                    br#"{"code":0,"msg":"ok","data":{"jobId":"j-slow"}}"#.to_vec()
                )),
            }))
            .cast(),
            poll,
            take,
            free,
        }
    }
    let vt: &'static MailVtable = Box::leak(Box::new(MailVtable { submit }));
    let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));

    let t0 = Instant::now();
    let env = b
        .submit("default", "{}".to_string(), vec![])
        .await
        .expect("信封");
    let elapsed = t0.elapsed();
    assert_eq!(env["data"]["jobId"], "j-slow", "{env}");
    assert!(
        elapsed >= Duration::from_millis(30),
        "pending 期间必须退避睡眠（20 × {:?}），实测 {elapsed:?} —— 空转（yield_now）会瞬回",
        crate::bridge::ffi::FFI_POLL_BACKOFF
    );
}

// ---------- 6.2：宿主侧配置（非密钥面） ----------

/// `smtp:` 段原样交给宿主：只吸收 profile 的 `allowed_*`（校验用），
/// `workers`/`queue_capacity`/连接字段（含凭据）一律不进宿主结构。
#[test]
fn mail_config_takes_only_non_secret_whitelist_face() {
    let v = json!({
        "workers": 4,
        "queue_capacity": 256,
        "default": {
            "host": "smtp.example.com", "port": 465, "tls": "tls",
            "mechanism": "login", "user": "u", "pass": "SECRET",
            "allowed_from": ["noreply@x.com"],
            "allowed_recipients": ["@x.com", "@partner.com"]
        },
        "mock": { "file_transport": "/tmp/eml" }
    });
    let cfg = MailConfig::from_value(&v).unwrap();
    assert_eq!(cfg.profile_keys(), vec!["default", "mock"]);
    let p = cfg.profile("default").unwrap();
    assert_eq!(p.allowed_from, vec!["noreply@x.com"]);
    assert_eq!(p.allowed_recipients, vec!["@x.com", "@partner.com"]);
    // 未声明白名单的 profile：空表（校验层 fail-closed 的依据）。
    assert!(cfg.profile("mock").unwrap().allowed_from.is_empty());
    assert!(cfg.profile("nope").is_none());
    // 非对象 profile / 非字符串数组 → 配置期明确报错（不静默吞）。
    assert!(MailConfig::from_value(&json!("x")).is_err());
    assert!(MailConfig::from_value(&json!({"a": 1})).is_err());
    assert!(
        MailConfig::from_value(&json!({"a": {"allowed_from": [1, 2]}})).is_err(),
        "allowed_from 必须是字符串数组"
    );
}

// ---------- 6.4：入参校验（纯函数，宿主权威层） ----------

#[test]
fn strip_crlf_removes_header_injection() {
    assert_eq!(strip_crlf("hi\r\nBcc: evil@x.com"), "hiBcc: evil@x.com");
    assert_eq!(strip_crlf("a\nb\rc"), "abc");
    assert_eq!(strip_crlf("clean"), "clean");
}

#[test]
fn address_validation_uses_lettre_parser() {
    assert!(validate_address("a@x.com").is_ok());
    assert!(validate_address("not-an-addr").is_err());
    assert!(validate_address("").is_err());
    assert!(validate_address("a@x.com>b@y.com").is_err());
    // CRLF 一律拒（地址无「合法换行」语义，不同于 subject 的剥离）。
    assert!(validate_address("a@x.com\r\nBcc: b@y.com").is_err());
    assert!(validate_address("a@x.com\n").is_err());
}

#[test]
fn whitelist_is_exact_and_fail_closed() {
    let p = MailProfileCfg {
        allowed_from: vec!["noreply@x.com".into()],
        allowed_recipients: vec!["@x.com".into(), "@partner.com".into()],
    };
    assert!(check_whitelist("noreply@x.com", &["a@x.com".into()], &p, "default").is_ok());
    assert!(check_whitelist("noreply@x.com", &["a@partner.com".into()], &p, "default").is_ok());
    // 大小写不敏感（域名大小写无语义）。
    assert!(check_whitelist("NoReply@X.com", &["A@X.com".into()], &p, "default").is_ok());
    let e = check_whitelist("evil@y.com", &["a@x.com".into()], &p, "default").unwrap_err();
    assert!(
        e.contains("allowed_from") && e.contains("evil@y.com"),
        "{e}"
    );
    let e = check_whitelist(
        "noreply@x.com",
        &["a@x.com".into(), "b@evil.com".into()],
        &p,
        "default",
    )
    .unwrap_err();
    assert!(
        e.contains("allowed_recipients") && e.contains("b@evil.com"),
        "{e}"
    );
    // 空表 = 拒绝（fail-closed，同 `tls: none` 的显式许可思路）。
    let empty = MailProfileCfg::default();
    let e = check_whitelist("a@x.com", &["b@x.com".into()], &empty, "default").unwrap_err();
    assert!(e.contains("allowed_from"), "{e}");
    let no_rcpt = MailProfileCfg {
        allowed_from: vec!["@x.com".into()],
        ..Default::default()
    };
    let e = check_whitelist("a@x.com", &["b@x.com".into()], &no_rcpt, "default").unwrap_err();
    assert!(e.contains("allowed_recipients"), "{e}");
}

/// B6：白名单未命中的文案**不得回显白名单内容** —— 回显等于让任何调用方换个 profile key
/// 就能枚举该 profile（乃至他 profile）的内域/客户域清单。只回「未命中 + profile 名」
/// + 下一步；回显调用方自己给的地址是可以的（那是它自己的输入）。
#[test]
fn whitelist_error_names_profile_without_echoing_the_list() {
    let p = MailProfileCfg {
        allowed_from: vec!["noreply@secret-corp.com".into()],
        allowed_recipients: vec!["@internal.corp".into()],
    };
    let e = check_whitelist("evil@y.com", &["a@internal.corp".into()], &p, "adm").unwrap_err();
    assert!(
        e.contains("adm")
            && e.contains("allowed_from")
            && e.contains("evil@y.com")
            && e.contains("下一步"),
        "{e}"
    );
    assert!(
        !e.contains("secret-corp.com") && !e.contains("internal.corp"),
        "不得回显白名单条目：{e}"
    );
    let e =
        check_whitelist("noreply@secret-corp.com", &["x@evil.com".into()], &p, "adm").unwrap_err();
    assert!(
        e.contains("allowed_recipients") && e.contains("x@evil.com") && e.contains("adm"),
        "{e}"
    );
    assert!(!e.contains("internal.corp"), "不得回显白名单条目：{e}");
}

/// B1：白名单是**唯一**越权控制点，故匹配必须是「域全等 / 地址全等」——
/// 裸后缀（`ends_with`）三处绕过逐一钉死（先红）：
/// ① `noreply@x.com` 放行同域仿冒 `evil-noreply@x.com`；
/// ② 漏写 `@` 的条目（`x.com`）放行跨域 `a@evilx.com`；
/// ③ 空条目 `""`（`ends_with("")` 恒真）= 白名单等于关闭。
/// 另钉「不做子域通配」：`@x.com` **不**命中 `a@sub.x.com`（子域须显式 `@sub.x.com`）。
#[test]
fn whitelist_blocks_the_three_suffix_bypasses() {
    let p = MailProfileCfg {
        allowed_from: vec!["noreply@x.com".into()],
        allowed_recipients: vec!["@x.com".into(), "@partner.com".into()],
    };
    // 正向对照（防「一刀切全拒」也被判绿）。
    assert!(check_whitelist("noreply@x.com", &["a@x.com".into()], &p, "default").is_ok());
    assert!(
        check_whitelist("NoReply@X.COM", &["a@PARTNER.com".into()], &p, "default").is_ok(),
        "大小写不敏感（域名/本地部大小写无语义）"
    );

    // ① 同域仿冒发件人：条目是完整地址 ⇒ 必须全等，不得后缀命中。
    let e = check_whitelist("evil-noreply@x.com", &["a@x.com".into()], &p, "default").unwrap_err();
    assert!(e.contains("allowed_from"), "{e}");

    // ② 漏写 `@` 的条目：既不命中 `a@evilx.com`，也不命中任何地址（fail-closed）。
    let bare = MailProfileCfg {
        allowed_from: vec!["noreply@x.com".into()],
        allowed_recipients: vec!["x.com".into()],
    };
    let e =
        check_whitelist("noreply@x.com", &["a@evilx.com".into()], &bare, "default").unwrap_err();
    assert!(e.contains("allowed_recipients"), "{e}");
    assert!(
        check_whitelist("noreply@x.com", &["a@x.com".into()], &bare, "default").is_err(),
        "裸域条目不得退化成后缀匹配（连本域也不放行）"
    );

    // ③ 空条目 = 白名单关闭：任何地址都不命中。
    let blank = MailProfileCfg {
        allowed_from: vec!["".into()],
        allowed_recipients: vec!["@x.com".into()],
    };
    let e = check_whitelist(
        "anyone@anywhere.com",
        &["a@x.com".into()],
        &blank,
        "default",
    )
    .unwrap_err();
    assert!(e.contains("allowed_from"), "{e}");

    // 子域**不**通配：`@x.com` 只覆盖本域；子域要显式列出。
    assert!(
        check_whitelist("noreply@x.com", &["a@sub.x.com".into()], &p, "default").is_err(),
        "@x.com 不得命中子域 a@sub.x.com"
    );
    let sub = MailProfileCfg {
        allowed_from: vec!["noreply@x.com".into()],
        allowed_recipients: vec!["@sub.x.com".into()],
    };
    assert!(
        check_whitelist("noreply@x.com", &["a@sub.x.com".into()], &sub, "default").is_ok(),
        "显式 @sub.x.com 必须命中"
    );
    assert!(
        check_whitelist("noreply@x.com", &["a@x.com".into()], &sub, "default").is_err(),
        "@sub.x.com 是域全等，不覆盖父域"
    );
}

/// B3：宿主 jobId 形态 —— `<16 hex 随机前缀>-<单调计数>`：不可猜（不是可枚举的
/// `{pid}-{seq}`，前缀每进程一次取自系统熵）、进程内唯一、前缀进程内稳定。
#[test]
fn host_job_id_is_unguessable_and_unique() {
    let a = next_job_id();
    let b = next_job_id();
    assert_ne!(a, b, "必须唯一");
    let (p1, s1) = a.rsplit_once('-').expect("形态 <prefix>-<seq>");
    let (p2, s2) = b.rsplit_once('-').expect("形态 <prefix>-<seq>");
    assert_eq!(p1, p2, "前缀每进程固定（同一把不可猜的票根）");
    assert_eq!(p1.len(), 16, "{a}");
    assert!(p1.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    assert!(
        s1.parse::<u64>().is_ok() && s2.parse::<u64>().is_ok(),
        "{a} / {b}"
    );
    assert!(
        s2.parse::<u64>().unwrap() > s1.parse::<u64>().unwrap(),
        "计数单调"
    );
    assert!(
        !a.starts_with(&format!("{}-", std::process::id())),
        "不得是可枚举的 pid 前缀：{a}"
    );
}

/// B1：条目格式在**装配期**校验（fail-fast）——非法条目让配置解析失败，而不是等到发信
/// 时才退化成「不命中」（那样运维只看到 code:5，不知是自己写错了白名单）。
/// 错误须点名：profile、字段、第几条、条目原文、原因、下一步。
#[test]
fn mail_config_rejects_malformed_whitelist_entries() {
    let cases: [(&str, &str); 5] = [
        ("", "空"),
        ("x.com", "'@'"),
        (" noreply@x.com", "空白"),
        ("@", "域"),
        ("not an address", "'@'"),
    ];
    for (bad, needle) in cases {
        let v = json!({"default": {"allowed_from": ["noreply@x.com", bad]}});
        let e = format!(
            "{}",
            MailConfig::from_value(&v).expect_err("非法白名单条目必须让装配期失败")
        );
        assert!(
            e.contains("default") && e.contains("allowed_from[1]"),
            "错误须点名 profile 与下标：{e}"
        );
        assert!(e.contains(needle), "错误须给出原因（{needle}）：{e}");
        assert!(e.contains("下一步"), "错误须给下一步：{e}");
    }
    // 合法两形态：完整地址 与 `@domain`。
    let ok = json!({"default": {
        "allowed_from": ["noreply@x.com"],
        "allowed_recipients": ["@x.com", "@partner.com"]
    }});
    let cfg = MailConfig::from_value(&ok).expect("合法白名单条目必须通过");
    assert_eq!(
        cfg.profile("default").unwrap().allowed_recipients,
        vec!["@x.com", "@partner.com"]
    );
}

// ---------- 6.4：附件引用解析 ----------

#[test]
fn attachment_refs_require_exactly_one_source() {
    let refs = parse_attachment_refs(&json!([
        {"filename": "a.pdf", "blobKey": "r2d2"},
        {"filename": "b.pdf", "path": "reports/b.pdf", "mime": "application/pdf", "blob": "img"},
    ]))
    .unwrap();
    assert_eq!(refs.len(), 2);
    assert_eq!(
        refs[0].src,
        AttSrc::Blob {
            name: "default".into(),
            key: "r2d2".into()
        }
    );
    assert_eq!(refs[0].mime, None);
    assert_eq!(refs[1].mime.as_deref(), Some("application/pdf"));
    assert_eq!(refs[1].src, AttSrc::Path("reports/b.pdf".into()));
    // 缺省（无 attachments / null）= 空表。
    assert!(parse_attachment_refs(&Value::Null).unwrap().is_empty());
    // 两个来源都给 / 都不给 / 缺 filename / 非对象 / 空 key → 全部拒绝。
    for bad in [
        json!([{"filename": "a", "blobKey": "k", "path": "p"}]),
        json!([{"filename": "a"}]),
        json!([{"blobKey": "k"}]),
        json!([{"filename": "", "blobKey": "k"}]),
        json!([{"filename": "a", "blobKey": ""}]),
        json!(["nope"]),
        json!({"filename": "a"}),
    ] {
        assert!(parse_attachment_refs(&bad).is_err(), "{bad}");
    }
}

#[test]
fn mime_resolution_prefers_explicit_then_ext_then_magic() {
    assert_eq!(
        resolve_mime(Some("application/x-custom"), "a.bin", b"%PDF-1.7"),
        "application/x-custom"
    );
    assert_eq!(resolve_mime(None, "r.pdf", b"whatever"), "application/pdf");
    assert_eq!(resolve_mime(None, "r.PDF", b""), "application/pdf");
    assert_eq!(resolve_mime(None, "n.csv", b""), "text/csv");
    // 扩展名未知 → 字节嗅探。
    assert_eq!(resolve_mime(None, "x.dat", b"%PDF-1.7"), "application/pdf");
    assert_eq!(
        resolve_mime(None, "x.dat", b"\x89PNG\r\n\x1a\n"),
        "image/png"
    );
    // 都不认识 → 兜底（插件侧同款默认）。
    assert_eq!(
        resolve_mime(None, "x.dat", b"hello"),
        "application/octet-stream"
    );
}

// ---------- 6.4：附件字节解析（blob / path） ----------

fn tmpdir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "oj-mail-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "current_thread")]
async fn resolve_attachments_reads_blob_and_path_in_declared_order() {
    use crate::bridge::blob::{self, BlobBackend, LocalBlob};
    let dir = tmpdir("att");
    let blob_root = dir.join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();
    let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
    lb.put("r2d2", b"BLOBBYTES", Some("application/pdf"))
        .await
        .unwrap();
    let reg = blob::registry_with_default(Arc::new(lb));
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join("reports")).unwrap();
    std::fs::write(proj.join("reports/x.pdf"), b"%PDF-1.7 file").unwrap();

    let refs = parse_attachment_refs(&json!([
        {"filename": "b.pdf", "path": "reports/x.pdf"},
        {"filename": "a.pdf", "blobKey": "r2d2", "mime": "application/custom"},
    ]))
    .unwrap();
    let atts = resolve_attachments(&reg, Some(&proj), &refs, AttachmentLimits::default())
        .await
        .unwrap();
    assert_eq!(atts.len(), 2);
    // 下标严格对齐（插件按下标取字节）。
    assert_eq!(atts[0].filename, "b.pdf");
    assert_eq!(atts[0].bytes, b"%PDF-1.7 file");
    assert_eq!(atts[0].mime, "application/pdf"); // 扩展名嗅探
    assert_eq!(atts[1].filename, "a.pdf");
    assert_eq!(atts[1].bytes, b"BLOBBYTES");
    assert_eq!(atts[1].mime, "application/custom"); // 显式优先

    // `../` 越界 → 拒绝（文案给下一步）。
    let esc = parse_attachment_refs(&json!([{"filename": "e", "path": "../outside.txt"}])).unwrap();
    let e = resolve_attachments(&reg, Some(&proj), &esc, AttachmentLimits::default())
        .await
        .unwrap_err();
    assert!(e.contains("附件路径") && e.contains("下一步"), "{e}");
    // 无 project root（loader 未配置）→ path 附件拒绝。
    let e = resolve_attachments(&reg, None, &refs, AttachmentLimits::default())
        .await
        .unwrap_err();
    assert!(e.contains("project root"), "{e}");
    // blob 后端未配置 / 键缺失 → 明确错误。
    let missing = parse_attachment_refs(&json!([{"filename": "m", "blobKey": "nope"}])).unwrap();
    let e = resolve_attachments(&reg, None, &missing, AttachmentLimits::default())
        .await
        .unwrap_err();
    assert!(e.contains("nope"), "{e}");
    let empty = blob::BlobRegistry::new();
    let e = resolve_attachments(&empty, None, &missing, AttachmentLimits::default())
        .await
        .unwrap_err();
    assert!(e.contains("not configured"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B2：附件上限（单附件 + 单封合计）必须**先于**入队放大生效：超限即 `code:5`，
/// 文案给下一步；path 路先看长度不读盘（`tokio::fs::metadata`），blob 路拿到字节即判。
/// 正向对照：正常大小的附件仍通过（防「一刀切拒绝」也被判绿）。
#[tokio::test(flavor = "current_thread")]
async fn resolve_attachments_enforces_per_file_and_total_limits() {
    use crate::bridge::blob::{self, BlobBackend, LocalBlob};
    let dir = tmpdir("limits");
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join("reports")).unwrap();
    // 上限设为 8 字节，便于用小文件覆盖所有分支。
    let limits = AttachmentLimits {
        max_attachment_bytes: 8,
        max_total_bytes: 12,
    };
    std::fs::write(proj.join("reports/ok.txt"), b"12345678").unwrap(); // == 上限：通过
    std::fs::write(proj.join("reports/big.txt"), b"123456789").unwrap(); // 超 1 字节
    std::fs::write(proj.join("reports/a.txt"), b"12345678").unwrap();
    std::fs::write(proj.join("reports/b.txt"), b"12345678").unwrap();
    let blob_root = dir.join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();
    let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
    BlobBackend::put(&lb, "big-blob", b"123456789", None)
        .await
        .unwrap();
    let reg = blob::registry_with_default(Arc::new(lb));

    // 单附件上限：path 路。
    let one =
        parse_attachment_refs(&json!([{"filename": "b.txt", "path": "reports/big.txt"}])).unwrap();
    let e = resolve_attachments(&reg, Some(&proj), &one, limits)
        .await
        .unwrap_err();
    assert!(
        e.contains("attachments[0]") && e.contains("9") && e.contains("8"),
        "错误须点名附件与两侧字节数：{e}"
    );
    assert!(
        e.contains("max_attachment_bytes") && e.contains("下一步"),
        "错误须给配置键与下一步：{e}"
    );
    // 单附件上限：blob 路（长度只有拿到字节才知道）。
    let one =
        parse_attachment_refs(&json!([{"filename": "b.bin", "blobKey": "big-blob"}])).unwrap();
    let e = resolve_attachments(&reg, Some(&proj), &one, limits)
        .await
        .unwrap_err();
    assert!(e.contains("max_attachment_bytes"), "{e}");

    // 单封合计上限：两个都在单件上限内（8+8），但合计 16 > 12 → 第二个即拒。
    let two = parse_attachment_refs(&json!([
        {"filename": "a.txt", "path": "reports/a.txt"},
        {"filename": "b.txt", "path": "reports/b.txt"},
    ]))
    .unwrap();
    let e = resolve_attachments(&reg, Some(&proj), &two, limits)
        .await
        .unwrap_err();
    assert!(
        e.contains("attachments[1]") && e.contains("max_total_attachment_bytes"),
        "错误须点名越界的那一个附件与合计上限键：{e}"
    );
    assert!(e.contains("下一步"), "{e}");

    // 正向对照：边界值（== 上限）与合计不超限时必须通过。
    let ok = parse_attachment_refs(&json!([
        {"filename": "a.txt", "path": "reports/a.txt"},
        {"filename": "ok.bin", "blobKey": "ok"},
    ]))
    .unwrap();
    let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
    BlobBackend::put(&lb, "ok", b"1234", None).await.unwrap();
    let reg = blob::registry_with_default(Arc::new(lb));
    let atts = resolve_attachments(&reg, Some(&proj), &ok, limits)
        .await
        .unwrap();
    assert_eq!(atts.len(), 2);
    assert_eq!(atts[0].bytes, b"12345678");
    assert_eq!(atts[1].bytes, b"1234");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B2：附件上限是**装配期**配置（`smtp.max_attachment_bytes` /
/// `max_total_attachment_bytes`，与 `workers` 同级）：缺省取默认，非正整数即报错
/// （0 等于禁止一切附件；写成字符串/负数都是写法错误，不静默取默认）。
#[test]
fn mail_config_parses_and_validates_attachment_limits() {
    let base = json!({"default": {"allowed_from": ["@x.com"]}});
    let cfg = MailConfig::from_value(&base).unwrap();
    assert_eq!(
        cfg.attachment_limits(),
        AttachmentLimits {
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES,
        }
    );
    assert_eq!(DEFAULT_MAX_ATTACHMENT_BYTES, 10 * 1024 * 1024);

    let mut v = base.clone();
    v["max_attachment_bytes"] = json!(1024);
    v["max_total_attachment_bytes"] = json!(2048);
    let cfg = MailConfig::from_value(&v).unwrap();
    assert_eq!(
        cfg.attachment_limits(),
        AttachmentLimits {
            max_attachment_bytes: 1024,
            max_total_bytes: 2048,
        }
    );
    // 上限键**不是** profile（不会因「缺 host」而被当 profile 解析失败）。
    assert_eq!(cfg.profile_keys(), vec!["default"]);

    for bad in [json!(0), json!(-1), json!("1024"), json!(1.5)] {
        let mut v = base.clone();
        v["max_attachment_bytes"] = bad.clone();
        let e = format!(
            "{}",
            MailConfig::from_value(&v).expect_err("非法附件上限必须让装配期失败")
        );
        assert!(
            e.contains("max_attachment_bytes") && e.contains("下一步"),
            "{bad} → {e}"
        );
    }
}

// ---------- 6.4：编排（校验 → 附件 → submit） ----------

fn whitelisted_config() -> MailConfig {
    MailConfig::new(HashMap::from([(
        "default".to_string(),
        MailProfileCfg {
            allowed_from: vec!["noreply@x.com".into()],
            allowed_recipients: vec!["@x.com".into()],
        },
    )]))
}

fn empty_blobs() -> Arc<crate::bridge::blob::BlobRegistry> {
    Arc::new(crate::bridge::blob::BlobRegistry::new())
}

/// 正常路：校验通过 → CRLF 剥离 → 地址规范化 → 开关按 op 覆写 → 原样转发给插件。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_normalizes_and_dispatches() {
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    let req = json!({
        "from": "noreply@x.com",
        "to": ["a@x.com"],
        "subject": "hi\r\nBcc: evil@y.com",
        "headers": {"X-Custom": "v\r\nX-Injected: 1"},
        "text": "body\r\nline2",
        "sync": true,          // JS 侧串用：宿主按 op 覆写
        "enqueue_only": true,
    })
    .to_string();
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &req,
        MailMode::Send,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 0);
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "default");
    let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
    assert_eq!(fwd["subject"], "hiBcc: evil@y.com"); // 剥离而非拒绝（design §10）
    assert_eq!(fwd["headers"]["X-Custom"], "vX-Injected: 1");
    assert_eq!(fwd["text"], "body\r\nline2"); // 正文不剥（正文换行有语义）
    assert_eq!(fwd["sync"], false, "宿主按 op 覆写开关");
    assert_eq!(fwd["enqueue_only"], false);
    assert_eq!(fwd["to"][0], "a@x.com");
}

/// B2（编排面）：超限附件经 `mail.*` 拿到 `{code:5}`（resolve，不 throw），且**不触达
/// 插件**（上限在宿主读盘处生效，故队列里不会出现这封信）；正常附件仍原样过线。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_rejects_oversized_attachment_with_code5() {
    let dir = tmpdir("limit-send");
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join("reports")).unwrap();
    std::fs::write(proj.join("reports/ok.txt"), b"12345678").unwrap();
    std::fs::write(proj.join("reports/big.txt"), b"123456789").unwrap();
    let cfg = MailConfig::with_limits(
        HashMap::from([(
            "default".to_string(),
            MailProfileCfg {
                allowed_from: vec!["noreply@x.com".into()],
                allowed_recipients: vec!["@x.com".into()],
            },
        )]),
        AttachmentLimits {
            max_attachment_bytes: 8,
            max_total_bytes: 16,
        },
    );
    let fake = FakeMail::new(cfg, Arc::new(Bus::new()));

    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        Some(&proj),
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
        "attachments": [
            {"filename": "a.txt", "path": "reports/ok.txt"},
            {"filename": "b.txt", "path": "reports/big.txt"}
        ]})
        .to_string(),
        MailMode::Send,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 5, "{env}");
    let msg = env["msg"].as_str().unwrap();
    assert!(
        msg.contains("max_attachment_bytes") && msg.contains("下一步"),
        "{env}"
    );
    assert!(
        fake.sent.lock().unwrap().is_empty(),
        "超限信不得触达插件（上限在宿主读盘处生效）"
    );

    // 正向对照：同配置下正常附件照常过线（字节与下标不变）。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        Some(&proj),
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                    "attachments": [{"filename": "a.txt", "path": "reports/ok.txt"}]})
        .to_string(),
        MailMode::Send,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 0, "{env}");
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].2[0].bytes, b"12345678");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 各 mode 的引擎开关：sendSync → sync、enqueue → enqueue_only（两者互斥不叠加）。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_sets_engine_flags_per_mode() {
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    let req = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"}).to_string();
    let expect = [
        (MailMode::Send, false, false),
        (MailMode::Sync, true, false),
        (MailMode::Enqueue, false, true),
    ];
    for (mode, _, _) in expect {
        handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req,
            mode,
            &CallCtx::default(),
        )
        .await;
    }
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 3);
    let flags: Vec<(bool, bool)> = sent
        .iter()
        .map(|(_, r, _)| {
            let v: Value = serde_json::from_str(r).unwrap();
            (
                v["sync"].as_bool().unwrap(),
                v["enqueue_only"].as_bool().unwrap(),
            )
        })
        .collect();
    let want: Vec<(bool, bool)> = expect.iter().map(|(_, s, e)| (*s, *e)).collect();
    assert_eq!(flags, want);
}

/// 校验失败一律 `{code:5}` 信封（不 throw）：地址非法 / 白名单未命中 / 未知 profile /
/// 缺收件人 / 头名越权 / 附件形态错 / 路径越界。校验优先于后端调用（后端不被触达）。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_rejects_with_code5_and_never_touches_backend() {
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    let proj = tmpdir("escape");
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({"from": "not-an-addr", "to": ["a@x.com"], "text": "x"}),
            "from",
        ),
        (
            json!({"from": "evil@y.com", "to": ["a@x.com"], "text": "x"}),
            "allowed_from",
        ),
        (
            json!({"from": "noreply@x.com", "to": ["b@evil.com"], "text": "x"}),
            "allowed_recipients",
        ),
        (
            json!({"from": "noreply@x.com", "to": [], "text": "x"}),
            "to",
        ),
        (json!({"from": "noreply@x.com", "to": ["a@x.com"]}), "正文"),
        (
            json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                       "headers": {"Subject": "hijack"}}),
            "headers",
        ),
        (
            json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                       "attachments": [{"filename": "a", "blobKey": "k", "path": "p"}]}),
            "blobKey",
        ),
        (
            json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                       "attachments": [{"filename": "a", "path": "../escape.pdf"}]}),
            "附件路径",
        ),
    ];
    for (req, needle) in cases {
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            Some(&proj),
            "default",
            &req.to_string(),
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        assert_eq!(env["code"], 5, "{req} → {env}");
        assert!(
            env["msg"].as_str().unwrap().contains(needle),
            "{req} → {env}"
        );
    }
    // 未知 profile（不回落 default：错配的 profile 名必须显式失败）。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "nope",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"}).to_string(),
        MailMode::Send,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 5, "{env}");
    assert!(env["msg"].as_str().unwrap().contains("nope"), "{env}");
    // 非对象请求 / 非法 JSON。
    for bad in [json!([1, 2]).to_string(), "{not json".to_string()] {
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &bad,
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        assert_eq!(env["code"], 5, "{bad} → {env}");
    }
    // 一次后端都没触达（校验在前）。
    assert!(fake.sent.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&proj);
}

/// 收件人地址非法一律 `{code:5}`：**to/cc/bcc 与 from 同一解析器**（`lettre::Address`，
/// 见 `address_list`），且错误**逐个字段点名**（`to[0]`/`cc[0]`/`bcc[0]`）便于定位。
/// 注意口径差异（design §10）：地址含 CRLF 是**拒绝**，不像 subject/headers 那样剥离
/// ——「含换行的合法地址」不存在，剥离只会把注入内容粘进地址。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_rejects_illegal_addresses_in_every_recipient_field() {
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    let base = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"});
    let cases: Vec<(Value, &str, &str)> = vec![
        (json!({"to": ["not-an-addr"]}), "to[0]", "地址非法"),
        (json!({"cc": ["not-an-addr"]}), "cc[0]", "地址非法"),
        (json!({"bcc": ["not-an-addr"]}), "bcc[0]", "地址非法"),
        // CRLF 注入地址 → 拒绝（而非剥离成 a@x.comBcc: evil@y.com）。
        (
            json!({"to": ["a@x.com\r\nBcc: evil@y.com"]}),
            "to[0]",
            "地址非法",
        ),
        (json!({"cc": ["a@x.com\n"]}), "cc[0]", "地址非法"),
        (json!({"bcc": ["a@x.com\r\n"]}), "bcc[0]", "地址非法"),
        // 元素/字段类型错也走同一 code:5（不静默丢弃该收件人）。
        (json!({"to": [1]}), "to[0]", "必须是字符串"),
        (json!({"cc": "a@x.com"}), "cc", "必须是字符串数组"),
        (json!({"bcc": [null]}), "bcc[0]", "必须是字符串"),
    ];
    for (patch, field, reason) in cases {
        let mut req = base.clone();
        for (k, v) in patch.as_object().unwrap() {
            req[k] = v.clone();
        }
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req.to_string(),
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        assert_eq!(env["code"], 5, "{req} → {env}");
        let msg = env["msg"].as_str().unwrap();
        assert!(msg.contains(field), "错误须点名 {field}：{env}");
        assert!(msg.contains(reason), "错误须给出原因 {reason}：{env}");
    }
    assert!(
        fake.sent.lock().unwrap().is_empty(),
        "地址校验失败不得触达后端"
    );
}

/// `headers` **不得覆盖结构化字段**（design §11：`From`/`To`/`Cc`/`Bcc`/`Subject` 决定
/// 信封与主题；B5 补 `Sender`/`Return-Path`/`Reply-To` 三个弱 spoof 面头）。这条是白名单
/// 绕过面：若放行 `To`，收件人白名单只查结构化 `to`，而信封由报头派生即可把信发给任意人。
/// 宿主侧对八个名字（大小写不敏感）一律 `{code:5}`。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_rejects_headers_covering_structured_fields() {
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    for name in [
        "From",
        "from",
        "FROM",
        "To",
        "to",
        "Cc",
        "cc",
        "Bcc",
        "bcc",
        "Subject",
        "subject",
        "Sender",
        "sender",
        "Return-Path",
        "return-path",
        "Reply-To",
        "reply-to",
    ] {
        let mut headers = serde_json::Map::new();
        headers.insert(name.to_string(), Value::String("evil@y.com".into()));
        let mut req = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"});
        req["headers"] = Value::Object(headers);

        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req.to_string(),
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        assert_eq!(env["code"], 5, "headers 覆盖 {name} 必须拒：{env}");
        let msg = env["msg"].as_str().unwrap();
        assert!(
            msg.contains("headers") && msg.contains(name),
            "错误须点明头名 {name}：{env}"
        );
    }
    assert!(fake.sent.lock().unwrap().is_empty());
}

/// 收件人白名单覆盖 **to ∪ cc ∪ bcc**（design §10）：任一字段越界即 `{code:5}`，且错误
/// 点名越界地址——防「只查 to、漏了 cc/bcc」的静默放行（Bcc 是典型绕过路径）。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_gates_cc_and_bcc_against_allowed_recipients() {
    let cfg = MailConfig::new(HashMap::from([(
        "default".to_string(),
        MailProfileCfg {
            allowed_from: vec!["noreply@x.com".into()],
            allowed_recipients: vec!["@x.com".into(), "@partner.com".into()],
        },
    )]));
    let fake = FakeMail::new(cfg, Arc::new(Bus::new()));

    for (patch, bad) in [
        (json!({"cc": ["c@evil.com"]}), "c@evil.com"),
        (json!({"bcc": ["b@evil.com"]}), "b@evil.com"),
    ] {
        let mut req = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"});
        for (k, v) in patch.as_object().unwrap() {
            req[k] = v.clone();
        }
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req.to_string(),
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        assert_eq!(env["code"], 5, "{req} → {env}");
        let msg = env["msg"].as_str().unwrap();
        assert!(
            msg.contains("allowed_recipients") && msg.contains(bad),
            "错误须点名越界收件人 {bad}：{env}"
        );
    }
    assert!(fake.sent.lock().unwrap().is_empty(), "越权信不得触达后端");

    // 正向对照：cc/bcc 都在白名单内 → 放行，且三字段原样过线
    // （防「一刀切拒绝 cc/bcc」的变异也被判绿）。
    let req = json!({"from": "noreply@x.com", "to": ["a@x.com"],
                         "cc": ["c@partner.com"], "bcc": ["b@x.com"], "text": "x"});
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &req.to_string(),
        MailMode::Send,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 0, "{env}");
    let sent = fake.sent.lock().unwrap().clone();
    let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
    assert_eq!(fwd["cc"][0], "c@partner.com");
    assert_eq!(fwd["bcc"][0], "b@x.com");
}

/// sendRaw：`raw` 必填且与 `attachments` 互斥；正文/主题不参与校验；from/to 仍校验。
#[tokio::test(flavor = "current_thread")]
async fn handle_send_raw_requires_raw_and_forbids_attachments() {
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    let req = json!({
        "from": "noreply@x.com",
        "to": ["a@x.com"],
        "raw": "Subject: s\r\nFrom: spoof@evil.com\r\n\r\nbody\r\n",
    })
    .to_string();
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &req,
        MailMode::Raw,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 0);
    let sent = fake.sent.lock().unwrap().clone();
    let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
    // raw 原文（含换行）原样转发：冲突头剥离是插件的职责。
    assert!(fwd["raw"].as_str().unwrap().contains("spoof@evil.com"));
    assert_eq!(fwd["sync"], false);
    assert_eq!(fwd["enqueue_only"], false);

    // 缺 raw。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"]}).to_string(),
        MailMode::Raw,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 5);
    assert!(env["msg"].as_str().unwrap().contains("raw"), "{env}");
    // raw 与附件互斥。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "raw": "X: 1\r\n\r\nb",
                    "attachments": [{"filename": "a", "blobKey": "k"}]})
        .to_string(),
        MailMode::Raw,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 5);
    assert!(
        env["msg"].as_str().unwrap().contains("attachments"),
        "{env}"
    );
    // B5：raw 与结构化 `headers` 互斥（原实现静默忽略 headers —— 调用方以为头加上了）。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "raw": "X: 1\r\n\r\nb",
                    "headers": {"X-Custom": "v"}})
        .to_string(),
        MailMode::Raw,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 5, "{env}");
    assert!(
        env["msg"].as_str().unwrap().contains("headers")
            && env["msg"].as_str().unwrap().contains("下一步"),
        "{env}"
    );
    // 空的 headers（`{}`/null）不算「给了」：raw 路照常可发（见下方正向对照）。
    // 结构化 subject 非空 → 覆盖原文 Subject（design §7：结构化为准）。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "subject": "override",
                    "raw": "Subject: orig\r\n\r\nb"})
        .to_string(),
        MailMode::Raw,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 0);
    let sent = fake.sent.lock().unwrap().clone();
    let fwd: Value = serde_json::from_str(&sent[1].1).unwrap();
    assert_eq!(fwd["subject"], "override");
    // 正向对照：空的 headers（`{}`）不算「给了」——raw 路照常可发（防「一刀切拒绝
    // 一切带 headers 字段的 raw」也被判绿）。
    let env = handle_send(
        fake.clone(),
        empty_blobs(),
        None,
        "default",
        &json!({"from": "noreply@x.com", "to": ["a@x.com"], "raw": "X: 1\r\n\r\nb",
                    "headers": {}})
        .to_string(),
        MailMode::Raw,
        &CallCtx::default(),
    )
    .await;
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(fake.sent.lock().unwrap().len(), 3, "失败路不触达后端");
}

// ---------- 6.5：JS 全局（Mail / mail）端到端 ----------

/// 带 mail（+ 可选 blob/loader）的 Bridge：JS 侧经全局 `mail.*` 打全套 op。
fn mail_bridge(
    fake: Arc<dyn MailBackend>,
    blobs: Option<Arc<BlobRegistry>>,
    root: Option<PathBuf>,
) -> Bridge {
    use crate::bridge::LoaderShared;
    Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        root.map(|project_root| {
            Arc::new(LoaderShared {
                project_root,
                ts: true,
            })
        }),
        Extras {
            mail: Some(fake),
            blobs,
            ..Default::default()
        },
    )
}

async fn run_js(b: &Bridge, src: &str) -> Value {
    run_js_req(b, src, crate::bridge::RequestInfo::default()).await
}

/// 带显式请求上下文（租户头）地跑一段 JS —— B3 的跨租户归属校验用例需要它。
async fn run_js_req(b: &Bridge, src: &str, req: crate::bridge::RequestInfo) -> Value {
    let cap = b.run_with(src, req).await.unwrap_or_else(|e| panic!("{e}"));
    serde_json::from_slice(&cap.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&cap.body)))
}

/// `mail.send`（全局）：JS 对象 → JSON → 宿主校验/附件解析 → 插件；信封原样回 JS。
/// 同批覆盖：`sendSync`/`enqueue` 的引擎开关、`Mail(key)` 实例 profile、CRLF 剥离。
#[tokio::test(flavor = "current_thread")]
async fn js_mail_send_and_instance_methods_hit_ops() {
    use crate::bridge::blob::{BlobBackend, LocalBlob};
    let _g = lock();
    let dir = tmpdir("js");
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join("reports")).unwrap();
    std::fs::write(proj.join("reports/x.pdf"), b"%PDF-1.7 body").unwrap();
    let blob_root = dir.join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();
    let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
    BlobBackend::put(&lb, "r2d2", b"BLOBBYTES", None)
        .await
        .unwrap();

    let cfg = MailConfig::new(HashMap::from([
        ("default".to_string(), {
            let mut p = whitelisted_config().profile("default").unwrap().clone();
            p.allowed_recipients.push("@partner.com".into());
            p
        }),
        (
            "alerts".to_string(),
            MailProfileCfg {
                allowed_from: vec!["alert@x.com".into()],
                allowed_recipients: vec!["@x.com".into()],
            },
        ),
    ]));
    let fake = FakeMail::new(cfg, Arc::new(Bus::new()));
    let b = mail_bridge(
        fake.clone(),
        Some(crate::bridge::blob::registry_with_default(Arc::new(lb))),
        Some(proj),
    );
    let v = run_js(
            &b,
            r#"(async () => {
                const r = await mail.send({
                  from: "noreply@x.com", to: ["a@x.com"],
                  subject: "hi\r\nBcc: evil@y.com",
                  headers: { "X-Custom": "v\r\nX-Injected: 1" },
                  text: "body",
                  attachments: [
                    { filename: "b.pdf", path: "reports/x.pdf" },
                    { filename: "a.bin", blobKey: "r2d2", mime: "application/x-custom" },
                  ],
                });
                const s = await mail.sendSync({ from: "noreply@x.com", to: ["c@partner.com"], text: "x" });
                const j = await mail.enqueue({ from: "noreply@x.com", to: ["a@x.com"], text: "x",
                                               jobId: "caller-controlled" });
                const a = await new Mail("alerts").send({ from: "alert@x.com", to: ["a@x.com"], text: "x" });
                json.ok({ r, s, j, a });
              })().catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
    assert!(v["data"].get("err").is_none(), "{v}");
    for k in ["r", "s", "j", "a"] {
        assert_eq!(v["data"][k]["code"], 0, "{k}: {v}");
    }
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 4);
    // B3：jobId 由**宿主**生成（非可猜的 {pid}-{seq}，也**不是**插件回显的 "j-stub"），
    // 且回执里的 id 与过线请求里的 id 是同一个（JS 拿到的票一定可 mail.result 回查）。
    let enq_req: Value = serde_json::from_str(&sent[2].1).unwrap();
    let job_id = enq_req["jobId"].as_str().expect("宿主必须给 jobId");
    assert_ne!(job_id, "caller-controlled", "调用方自带的 jobId 必须被剥掉");
    assert_eq!(
        v["data"]["j"]["data"]["jobId"], job_id,
        "回执的 jobId 必须钉成宿主值（不依赖插件回显）"
    );
    let (prefix, seq) = job_id.rsplit_once('-').expect("jobId 形态 <hex>-<seq>");
    assert_eq!(prefix.len(), 16, "随机前缀 16 hex：{job_id}");
    assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()), "{job_id}");
    assert!(seq.parse::<u64>().is_ok(), "{job_id}");
    // 附件：下标序 + 字节 + MIME（显式优先 / 扩展名嗅探）。
    assert_eq!(sent[0].2.len(), 2);
    assert_eq!(sent[0].2[0].filename, "b.pdf");
    assert_eq!(sent[0].2[0].bytes, b"%PDF-1.7 body");
    assert_eq!(sent[0].2[0].mime, "application/pdf");
    assert_eq!(sent[0].2[1].filename, "a.bin");
    assert_eq!(sent[0].2[1].bytes, b"BLOBBYTES");
    assert_eq!(sent[0].2[1].mime, "application/x-custom");
    // CRLF 剥离在宿主侧完成（插件拿到的是消毒后的请求）。
    let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
    assert_eq!(fwd["subject"], "hiBcc: evil@y.com");
    assert_eq!(fwd["headers"]["X-Custom"], "vX-Injected: 1");
    // 引擎开关按方法覆写；profile key 按实例走。
    let f1: Value = serde_json::from_str(&sent[1].1).unwrap();
    assert_eq!(
        (f1["sync"].as_bool(), f1["enqueue_only"].as_bool()),
        (Some(true), Some(false))
    );
    let f2: Value = serde_json::from_str(&sent[2].1).unwrap();
    assert_eq!(
        (f2["sync"].as_bool(), f2["enqueue_only"].as_bool()),
        (Some(false), Some(true))
    );
    assert_eq!(sent[3].0, "alerts");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 校验失败经 JS 拿到 `{code:5}`（resolve，不抛）；`mail not configured` 才抛异常。
#[tokio::test(flavor = "current_thread")]
async fn js_mail_validation_envelope_and_not_configured_throw() {
    let _g = lock();
    let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
    let b = mail_bridge(fake.clone(), None, None);
    let v = run_js(
            &b,
            r#"(async () => {
                const bad = await mail.send({ from: "noreply@x.com", to: ["b@evil.com"], text: "x" });
                const raw = await mail.sendRaw({ from: "noreply@x.com", to: ["a@x.com"] });
                json.ok({ bad, raw });
              })().catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
    assert_eq!(v["data"]["bad"]["code"], 5, "{v}");
    assert!(
        v["data"]["bad"]["msg"]
            .as_str()
            .unwrap()
            .contains("allowed_recipients"),
        "{v}"
    );
    assert_eq!(v["data"]["raw"]["code"], 5, "{v}");
    assert!(fake.sent.lock().unwrap().is_empty());

    // 未配置（无 smtp/插件）→ 抛明确错误（不 panic、不静默）。
    let plain = Bridge::new(
        Arc::new(InMemoryAccessor::new()),
        Arc::new(InMemoryKV::new()),
    );
    let v = run_js(
            &plain,
            r#"(async () => { await mail.send({ from: "a@x.com", to: ["b@x.com"], text: "x" }); json.ok({}); })()
                 .catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
    assert!(
        v["data"]["err"]
            .as_str()
            .unwrap()
            .contains("mail not configured"),
        "{v}"
    );
}

/// `mail.result(id)`（异步上送结果）+ `Mail.profiles()`（非密钥面）+ B3 归属校验：
/// - 本归属（同 profile / 同租户）→ 结果；
/// - 换 profile（`new Mail("alerts")`）或换租户（请求头）→ `null`（不泄露存在性）；
/// - 未命中 / 已过期 → `null`。
#[tokio::test(flavor = "current_thread")]
async fn js_mail_result_is_scoped_to_caller_owner() {
    let _g = lock();
    let cfg = MailConfig::new(HashMap::from([
        (
            "default".to_string(),
            whitelisted_config().profile("default").unwrap().clone(),
        ),
        (
            "alerts".to_string(),
            MailProfileCfg {
                allowed_from: vec!["noreply@x.com".into()],
                allowed_recipients: vec!["@x.com".into()],
            },
        ),
    ]));
    let fake = FakeMail::new(cfg, Arc::new(Bus::new()));
    let b = mail_bridge(fake.clone(), None, None);
    // 三张票：j1（default / 无租户）、j2（alerts / 无租户）、j3（default / 租户 t1）。
    let env = |job_id: &str| {
        format!(
            r#"{{"code":0,"msg":"sent","data":{{"jobId":"{job_id}","messageId":"m-{job_id}"}}}}"#
        )
    };
    assert!(fake.router().reserve("j1", owner()));
    assert!(
        fake.router()
            .reserve("j2", MailOwner::new("alerts", &CallCtx::default()))
    );
    assert!(fake.router().reserve(
        "j3",
        MailOwner::new(
            "default",
            &CallCtx {
                module: None,
                tenant: Some("t1".into()),
            }
        )
    ));
    for id in ["j1", "j2", "j3"] {
        deliver_to_host(MAIL_RESULT_TOPIC, env(id).as_bytes());
    }
    let js = r#"(async () => {
                const r = await mail.result("j1");
                const alienProfile = await mail.result("j2");
                const alienTenant = await mail.result("j3");
                const alertsOwn = await new Mail("alerts").result("j2");
                const miss = await mail.result("nope");
                const p = await Mail.profiles();
                json.ok({ r, alienProfile, alienTenant, alertsOwn, miss, p });
              })().catch((e) => json.ok({ err: String(e) }));
              "#;
    // 无租户上下文：j1 命中；j2（他人 profile）与 j3（他人租户）都不可见。
    let v = run_js(&b, js).await;
    assert!(v["data"].get("err").is_none(), "{v}");
    assert_eq!(v["data"]["r"]["code"], 0, "{v}");
    assert_eq!(v["data"]["r"]["messageId"], "m-j1");
    assert!(v["data"]["r"].get("subject").is_none(), "{v}");
    assert_eq!(
        v["data"]["alienProfile"],
        Value::Null,
        "跨 profile 必须不可见：{v}"
    );
    assert_eq!(
        v["data"]["alienTenant"],
        Value::Null,
        "跨租户必须不可见：{v}"
    );
    assert_eq!(
        v["data"]["alertsOwn"]["messageId"], "m-j2",
        "本 profile 必须可见：{v}"
    );
    assert_eq!(v["data"]["miss"], Value::Null);
    assert_eq!(v["data"]["p"], json!(["alerts", "default"]));

    // 同租户上下文（t1）→ j1/j3 都可见（归属一致），j2 仍不可见（profile 不同）。
    let v = run_js_req(
        &b,
        js,
        crate::bridge::RequestInfo {
            tenant_id: Some("t1".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(v["data"]["r"], Value::Null, "j1 属于无租户归属：{v}");
    assert_eq!(v["data"]["alienTenant"]["messageId"], "m-j3", "{v}");
}
