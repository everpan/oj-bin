//! mail FileTransport 端到端：**真装配**（`smtp:` 段 + `oj-mail` 插件 + `App::from_config`）
//! → HTTP 路由里的 JS `mail.send` / `mail.enqueue` → 真 `.eml` 落盘 + 真结果回查。
//!
//! 夹具（config.yaml / 项目 / `boot` / `get`）见 `mail_common`；覆盖链与前置（
//! `cargo xtask plugin mail`）亦见该模块头。
//!
//! **单独测试目标的原因**：`oj-mail` 的引擎是**进程级单例**（插件侧 `MAIL_ENGINE`
//! `OnceLock`，首次 init 钉死 transport 与 `file_transport` 目录），与其它 mail 用例
//! 共进程会互相污染；停机排空（会永久停掉该单例）另起 `mail_drain_e2e.rs`。

// lock() 的 std MutexGuard 有意全程持有（本目标用例串行，横跨 await 是设计而非疏漏）。
#![allow(clippy::await_holding_lock)]

mod mail_common;

use mail_common::{
    ATT_PAYLOAD, ATT_PAYLOAD_B64, boot, eml_count, eml_dir, get, lock, tmp_project, write_project,
};

/// Given: `smtp.mock`（FileTransport）+ oj-mail 装好；
/// When: 路由里 `mail.send` 带 `{path}` 附件发一封信；
/// Then: 信封 `code==0` 且带 `messageId`；该 `messageId` 命名的 `.eml` 真落盘，
/// 内容含结构化 From/To/Subject 与正文；附件**原始字节**在场且未被 base64 化。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_send_writes_eml_and_returns_envelope() {
    let _g = lock();
    let t = tmp_project();
    let src = write_project(&t);
    let app = boot(&t, &src).await;
    // 装配点证据：StableState.mail 与 actor 的 Extras.mail 同源注入（未配 smtp/未装插件
    // 时这里会是 None，随后由 op 层抛 "mail not configured"）。
    assert!(
        app.stable().mail.is_some(),
        "smtp: 段 + oj-mail 在册 → StableState.mail 必须就位"
    );

    let (status, env) = get(&app, "/v1/api/mail/").await;
    assert_eq!(status, 200, "{env}");
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(env["msg"], "ok", "{env}");
    let message_id = env["data"]["messageId"]
        .as_str()
        .unwrap_or_else(|| panic!("信封必须带投递凭据 messageId：{env}"));
    assert!(
        !env["data"]["jobId"].as_str().unwrap_or("").is_empty(),
        "{env}"
    );

    // 按 messageId 命名读回（FileTransport 的落盘名 = 投递凭据）→ 并发/多封信也不串。
    let eml_path = eml_dir().join(format!("{message_id}.eml"));
    let eml = std::fs::read_to_string(&eml_path)
        .unwrap_or_else(|e| panic!("{} 未落盘（{e}）", eml_path.display()));
    assert!(eml.contains("From: noreply@x.com"), "{eml}");
    assert!(eml.contains("To: a@x.com"), "{eml}");
    assert!(eml.contains("Subject: oj mail e2e"), "{eml}");
    assert!(eml.contains("hello from e2e"), "{eml}");
    // 附件：文件名 + 原始字节在场；base64 形态**不在**（非「base64 化成文本流」）。
    assert!(eml.contains("note.txt"), "{eml}");
    assert!(eml.contains(ATT_PAYLOAD), "附件原始字节必须在场：{eml}");
    assert!(
        !eml.contains(ATT_PAYLOAD_B64),
        "附件字节不得被 base64 化成文本流：{eml}"
    );
    let _ = std::fs::remove_file(&eml_path);
}

/// Given: 同一装配，`from` 越出 `allowed_from` 白名单；
/// Then: `{code:5}`（宿主**权威**校验层拦下，不触达插件）；未落任何 `.eml`
/// ——白名单 fail-closed 的第一道实证（纵深防御仍在插件侧）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_send_from_outside_whitelist_returns_code5() {
    let _g = lock();
    let t = tmp_project();
    let src = write_project(&t);
    let app = boot(&t, &src).await;

    let before = eml_count();
    let (status, env) = get(&app, "/v1/api/mail/?from=evil%40y.com").await;
    assert_eq!(status, 200, "{env}");
    assert_eq!(env["code"], 5, "{env}");
    let msg = env["msg"].as_str().unwrap_or_default();
    assert!(msg.contains("allowed_from"), "{env}");
    assert!(msg.contains("evil@y.com"), "{env}");
    assert_eq!(eml_count(), before, "越权信不得落盘：{env}");
}

/// Given: 同一**真装配**（真 oj-mail 插件，非 FakeMail）+ `mail.enqueue`；
/// Then: ① `enqueue` 回**统一信封**（`code:0`、`data.jobId` 是字符串、**顶层无裸 jobId**）；
/// ② 同队列的 `send` 作屏障后，`mail.result(jobId)` 命中该 job 的完成结果（`code:0` + `messageId`）
/// —— A1 的核心：拿得到 jobId 才回查得到结果（裸形态会让 `res.data.jobId` TypeError）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_enqueue_returns_envelope_and_result_is_queryable() {
    let _g = lock();
    let t = tmp_project();
    let src = write_project(&t);
    let app = boot(&t, &src).await;

    let before = eml_count();
    let (status, v) = get(&app, "/v1/api/mail/?mode=enqueue").await;
    assert_eq!(status, 200, "{v}");

    // ① enqueue 的立即回执 = 统一信封（**非**裸 `{jobId}`）。
    let enq = &v["enq"];
    assert_eq!(enq["code"], 0, "enqueue 必须回统一信封：{v}");
    assert_eq!(enq["msg"], "ok", "{v}");
    assert_eq!(
        v["jobIdType"], "string",
        "data.jobId 必须是字符串（用户按手册写 res.data.jobId）：{v}"
    );
    assert!(
        enq.get("jobId").is_none(),
        "顶层不得回裸 jobId（契约是 data.jobId）：{v}"
    );
    let mut top: Vec<&str> = enq
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    top.sort_unstable();
    assert_eq!(top, ["code", "data", "msg"], "{v}");

    // 屏障：send 的结果 resolve ⇒ 先入队的 enqueue 那封已完成并上送宿主。
    assert_eq!(v["barrier"]["code"], 0, "屏障 send 必须成功：{v}");

    // ② mail.result(jobId) 取到完成结果（含投递凭据 messageId）→ 真落盘。
    let res = &v["res"];
    let job_id = v["jobId"].as_str().expect("jobId");
    assert_eq!(res["jobId"], job_id, "结果须按同一 jobId 索引：{v}");
    assert_eq!(res["code"], 0, "结果信封：{v}");
    let message_id = res["messageId"]
        .as_str()
        .unwrap_or_else(|| panic!("结果必须带投递凭据 messageId：{v}"));
    let eml_path = eml_dir().join(format!("{message_id}.eml"));
    let eml = std::fs::read_to_string(&eml_path)
        .unwrap_or_else(|e| panic!("{} 未落盘（{e}）", eml_path.display()));
    assert!(eml.contains("enqueued"), "enqueue 那封的正文：{eml}");
    // 两封（enqueue + 屏障 send）都落盘。
    assert_eq!(eml_count(), before + 2, "{v}");
}
