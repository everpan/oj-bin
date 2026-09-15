//! mail **停机排空**（A4）端到端：真装配 + 真 `oj-mail` 插件 + **生产停机入口**
//! （`App::drain_mail` → `MailBackend::drain` → 控制报文 `{"__ctl":"drain"}` → 插件
//! `MailEngine::drain`）。
//!
//! **必须单独测试目标**：drain 会永久停掉插件内**进程级单例**引擎（此后 `submit` 一律
//! 停机错误），与 `mail_e2e.rs` 的投递用例共进程会互相污染。

#![allow(clippy::await_holding_lock)]

mod mail_common;

use std::time::Duration;

use mail_common::{boot, get, lock, tmp_project, try_boot, write_project};

/// A5：双配置源（非空 `smtp:` ＋ 非空 `plugins.mail`）在**真装配**期 fail-fast ——
/// `plugin_cfg` 会让 `plugins.mail` 静默胜出，运维改 `smtp:` 会「改了不生效」。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_mail_cfg_sources_fail_fast_at_boot() {
    let _g = lock();
    let t = tmp_project();
    let src = write_project(&t);
    // 夹具默认是 `plugins: {mail: {}}`（空对象 = 回落适配器）；改成**非空透传**即双源冲突。
    let p = t.0.join("config.yaml");
    let cfg = std::fs::read_to_string(&p)
        .unwrap()
        .replace("  mail: {}\n", "  mail:\n    mock:\n      host: override\n");
    std::fs::write(&p, cfg).unwrap();

    let e = match try_boot(&t, &src).await {
        Ok(_) => panic!("双源非空必须装配失败"),
        Err(e) => e, // App 非 Debug，不能用 expect_err
    };
    assert!(
        e.contains("plugins.mail") && e.contains("pick one"),
        "文案须点明二选一：{e}"
    );
}

/// Given: `smtp.mock`（FileTransport）+ oj-mail 装好，先真投递一封（证明引擎在工作）；
/// When: 走**生产停机路径** `App::drain_mail`（控制报文 drain，HTTP 已停收后的调用点同款）；
/// Then: ① 控制报文真到达插件并把它停掉 —— 排空后新投递被拒（宿主把 FFI 的停机错误收敛为
/// `{code:1}` 且文案点名「停机」）；② 排空前的在途邮件已真落盘（drain 不是丢件）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_reaches_plugin_and_stops_accepting_new_mail() {
    let _g = lock();
    let t = tmp_project();
    let src = write_project(&t);
    let app = boot(&t, &src).await;

    // ① 排空前：真投递一封（真 .eml 落盘）。
    let (status, env) = get(&app, "/v1/api/mail/").await;
    assert_eq!(status, 200, "{env}");
    assert_eq!(env["code"], 0, "{env}");
    let message_id = env["data"]["messageId"].as_str().expect("messageId");
    assert!(
        mail_common::eml_dir()
            .join(format!("{message_id}.eml"))
            .is_file(),
        "排空前那封必须真落盘：{env}"
    );

    // ② 生产停机入口（server_cmd 在 HTTP 停收 + 任务收场之后调它）。
    app.drain_mail(Duration::from_secs(5)).await;

    // ③ 排空后插件不再收投递 —— 这是「控制报文真到达插件」的行为证据
    //    （宿主 handle_send 把 FFI 的 `engine 已停机` 错误收敛为 code:1）。
    let (status, env2) = get(&app, "/v1/api/mail/").await;
    assert_eq!(status, 200, "{env2}");
    assert_eq!(env2["code"], 1, "drain 后新投递必须被拒：{env2}");
    let msg = env2["msg"].as_str().unwrap_or_default();
    assert!(
        msg.contains("停机"),
        "文案须点名插件已停机（证明控制报文到达插件）：{env2}"
    );

    // ④ drain 幂等：再调一次不 panic、不挂死（超时上限内返回）。
    app.drain_mail(Duration::from_secs(5)).await;
}
