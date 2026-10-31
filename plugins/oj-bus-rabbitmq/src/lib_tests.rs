#[test]
fn mask_url_hides_password_keeps_plain() {
    assert_eq!(mask_url("amqp://u:p@h:1/v"), "amqp://u:***@h:1/v");
    assert_eq!(mask_url("amqp://h:1"), "amqp://h:1");
    assert_eq!(mask_url("bad"), "bad");
}

/// timeoutMs（camelCase JS 面）必须真正生效——评审 must-fix：此前 serde 静默丢弃。
#[test]
fn poll_req_accepts_js_camel_case_timeout() {
    let req: PollReq =
        serde_json::from_value(serde_json::json!({ "queues": ["q"], "timeoutMs": 1234 })).unwrap();
    assert_eq!(req.timeout_ms, 1234);
    // 缺省回落 d_timeout；snake alias 兼容。
    let d: PollReq = serde_json::from_value(serde_json::json!({ "queues": ["q"] })).unwrap();
    assert_eq!(d.timeout_ms, d_timeout());
    let a: PollReq =
        serde_json::from_value(serde_json::json!({ "queues": ["q"], "timeout_ms": 77 })).unwrap();
    assert_eq!(a.timeout_ms, 77);
}

use super::*;

// ---- mq 面（TDD 先行用例）----

#[test]
fn given_wrong_kind_when_mq_connect_then_err_names_kind() {
    // Given: cfg.kind = "kafka"（装错插件）；Then: Err 点名 kind（评审 F7）
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let cfg = serde_json::json!({ "kind": "kafka", "url": "amqp://127.0.0.1:5672" }).to_string();
    let rt = runtime();
    let out = rt.block_on(drive(&mut mq_connect(RString::from(cfg.as_str()))));
    assert!(matches!(out, Err(ref e) if e.contains("kind")), "{out:?}");
}

#[test]
fn given_unknown_method_when_mq_call_then_err_unsupported() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let rt = runtime();
    let out = rt.block_on(drive(&mut mq_call(
        1,
        RString::from("nope"),
        RString::from("{}"),
    )));
    assert!(
        matches!(out, Err(ref e) if e.contains("unsupported method: nope")),
        "{out:?}"
    );
}

#[test]
fn given_mq_cfg_without_url_when_connect_then_err_fail_fast() {
    // Given: kind 正确但缺 url/brokers → lapin 无法拨号 → cfg 校验 fail-fast
    //（rabbit 装配期即可探活，评审 S3 裁决）
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let cfg = serde_json::json!({ "kind": "rabbit" }).to_string();
    let rt = runtime();
    let out = rt.block_on(drive(&mut mq_connect(RString::from(cfg.as_str()))));
    assert!(
        matches!(out, Err(ref e) if e.contains("url") || e.contains("brokers")),
        "{out:?}"
    );
}

/// 真 rabbitmq mq 面 roundtrip（env-gated）：send → poll → ack。
#[tokio::test(flavor = "multi_thread")]
async fn given_real_rabbitmq_when_mq_send_poll_ack_then_roundtrips() {
    let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
            return;
        }
    };
    let cfg = serde_json::json!({ "kind": "rabbit", "url": url }).to_string();
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut mq_connect(RString::from(cfg.as_str())))
        .await
        .expect("mq connect");
    let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    // 声明队列（mq 面消费既有具名队列；默认交换 direct 投递）。
    // 队列须先存在：basic_get/publish 到不存在队列会 404/静默丢弃——mq 面
    // 无 queue_declare 方法，测试自备：直接用 lapin 声明（不依赖外部 init）。
    let queue = format!("oj-mq-{}", std::process::id());
    let setup = lapin::Connection::connect(&url, lapin::ConnectionProperties::default())
        .await
        .expect("test rabbit conn");
    // RabbitMQ 4.x 默认禁止「瞬时非排他队列」（durable=false & exclusive=false）→
    // 声明即 INTERNAL_ERROR。测试队列设 durable=true（持久）转合法队列；并设
    // auto_delete=true 在消费者断开后回收，贴合 roundtrip 语义且不留垃圾。
    let qopts = lapin::options::QueueDeclareOptions {
        durable: true,
        auto_delete: true,
        ..Default::default()
    };
    setup
        .create_channel()
        .await
        .expect("test rabbit channel")
        .queue_declare(queue.as_str(), qopts, lapin::types::FieldTable::default())
        .await
        .expect("declare queue");
    let send_payload = serde_json::json!({
        "exchange": "", "routingKey": queue,
        "headers": {"h1": "v1"},
        "value": {"n": 1},
    })
    .to_string();
    drive(&mut mq_call(
        handle,
        RString::from("send"),
        RString::from(send_payload.as_str()),
    ))
    .await
    .expect("mq send");
    let polled = drive(&mut mq_call(
        handle,
        RString::from("poll"),
        RString::from(
            serde_json::json!({ "queues": [queue], "max": 10, "timeoutMs": 5000 })
                .to_string()
                .as_str(),
        ),
    ))
    .await
    .expect("mq poll");
    let v: serde_json::Value = serde_json::from_slice(&polled).unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 1, "{v}");
    drive(&mut mq_call(
        handle,
        RString::from("ack"),
        RString::from(v["messages"][0].to_string().as_str()),
    ))
    .await
    .expect("mq ack");
    // 二进制载荷（v0.1.16）：value_b64 走 base64 透传，poll 端见到 value_b64 且
    // value=null（覆盖 `poll` 的二进制分支）。
    let bin = b"\xff\xfe\x00\x01non-utf8";
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bin)
    };
    let bin_payload = serde_json::json!({
        "exchange": "", "routingKey": queue,
        "value_b64": b64,
    })
    .to_string();
    drive(&mut mq_call(
        handle,
        RString::from("send"),
        RString::from(bin_payload.as_str()),
    ))
    .await
    .expect("mq send bin");
    let polled2 = drive(&mut mq_call(
        handle,
        RString::from("poll"),
        RString::from(
            serde_json::json!({ "queues": [queue], "max": 10, "timeoutMs": 5000 })
                .to_string()
                .as_str(),
        ),
    ))
    .await
    .expect("mq poll bin");
    let v2: serde_json::Value = serde_json::from_slice(&polled2).unwrap();
    assert_eq!(v2["messages"].as_array().unwrap().len(), 1, "{v2}");
    assert!(v2["messages"][0]["value_b64"].is_string(), "{v2}");
    assert_eq!(v2["messages"][0]["value"], serde_json::Value::Null);
    drive(&mut mq_call(
        handle,
        RString::from("ack"),
        RString::from(v2["messages"][0].to_string().as_str()),
    ))
    .await
    .expect("mq ack bin");
    mq_close(handle);
}

// ---- bus 面（既有用例零改动 = 回归护栏）----

/// cfg 校验离线路径：url 与 brokers 皆缺 → fail-fast。
#[test]
fn rabbitmq_requires_url_or_brokers() {
    let cfg = BrokerCfgJson::default();
    let st = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = st.block_on(RabbitBroker::new(&cfg));
    assert!(r.is_err());
}

/// 真 rabbitmq roundtrip（env-gated）：`OJ_TEST_RABBITMQ_URL` 给 amqp URL。
/// 未设置 → 跳过（不进网络）。
#[tokio::test(flavor = "multi_thread")]
async fn real_rabbitmq_publish_subscribe_roundtrip() {
    let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
            return;
        }
    };
    let cfg = serde_json::json!({
        "kind": "rabbitmq",
        "brokers": [],
        "url": url,
        "group": null,
        "topic_prefix": format!("ojtest-{}", std::process::id()),
    })
    .to_string();
    let desc = match std::result::Result::from(init(host(), RString::from("{}"))) {
        Ok(d) => d,
        Err(e) => panic!("init failed: {}", &e[..]),
    };
    assert_eq!(&desc.name[..], "bus-rabbitmq");

    let bytes = drive(&mut connect(RString::from(cfg.as_str())))
        .await
        .expect("connect");
    let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    let topic = format!("t.{}", std::process::id());
    drive(&mut subscribe(handle, RString::from(topic.as_str())))
        .await
        .expect("subscribe");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drive(&mut publish(
        handle,
        RString::from(topic.as_str()),
        to_rbytes(br#"{"topic":"t","data":{"hi":1}}"#),
    ))
    .await
    .expect("publish");
    // 消费循环经 deliver 回调上送（真实扇出语义由宿主侧 ffi.rs 适配器测试覆盖）。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    close(handle);
}

extern "C" fn test_log(_level: u8, _msg: RString) {}
extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

fn host() -> RArc<HostContext> {
    RArc::new(HostContext {
        log: test_log,
        deliver: test_deliver,
    })
}

/// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
    // 以真实墙钟时间为界轮询（同 oj-es 的 drive）：固定 10w 次 yield_now 在 CI
    // 负载/优化下会在插件 rt 的任务完成前耗尽预算，误报 "ffi drive timeout"。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match (fut.poll)(fut.state) {
            0 => {
                if std::time::Instant::now() >= deadline {
                    (fut.free)(fut.state); // 超时也要释放 state（防 FfiTask 泄漏）
                    fut.state = std::ptr::null_mut();
                    return Err("ffi drive timeout".into());
                }
                tokio::time::sleep(std::time::Duration::from_micros(100)).await;
            }
            code => {
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut();
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Err(e)) => Err(e[..].to_string()),
                    _ => Err("ffi drive timeout".into()),
                };
            }
        }
    }
}
