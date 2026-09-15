//! mail FileTransport 端到端：**真装配**（`smtp:` 段 + `oj-mail` 插件 + `App::from_config`）
//! → HTTP 路由里的 JS `mail.send` → 真 `.eml` 落盘。
//!
//! 覆盖链：config.yaml 解析（`SmtpSection`）→ `plugin_cfg("mail")` 适配器 → 插件 init
//! （建 transport）→ `build_mail_backend`（宿主白名单面）→ `Extras.mail`/`StableState.mail`
//! → 路由 + 真 runtime 的 JS 全局 `mail` → vtable `submit` → 引擎有界队列 + worker
//! → lettre `FileTransport` 写 `.eml`。
//!
//! **前置**：`cargo xtask plugin mail`（cdylib 归置到 `bin/plugins/<host-triple>/`）。
//! 与 `oidc_e2e.rs` 同款：插件不由测试编译、缺失即启动期明确失败（指向该命令），
//! 全程不依赖网络（FileTransport 落盘，零 SMTP 连接）。
//!
//! **单独测试目标的原因**：`oj-mail` 的引擎是**进程级单例**（插件侧 `MAIL_ENGINE`
//! `OnceLock`，首次 init 钉死 transport 与 `file_transport` 目录），与其它 mail 用例
//! 共进程会互相污染；本目标内两条用例亦串行并共用同一落盘目录。

// lock() 的 std MutexGuard 有意全程持有（本目标用例串行，横跨 await 是设计而非疏漏）。
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use axum::body::Body;
use axum::http::Request;
use only_js::config::Config;

fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 落盘目录（**全目标共用**）：插件引擎只认首次 init 的 `file_transport`，故两条用例
/// 的 `smtp:` 段必须逐字相同（同一目录）——重复 init 由插件幂等吸收。
fn eml_dir() -> &'static PathBuf {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("oj-mail-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
}

/// `<workspace_root>/bin/plugins`（xtask 归置目录）：两候选谁存在用谁；都不存在留 None
/// → 装配期报「找不到插件清单」并指向 `cargo xtask plugin mail`。
fn plugins_dir() -> Option<PathBuf> {
    ["../bin/plugins", "../../../../bin/plugins"]
        .iter()
        .map(|p| Path::new(env!("CARGO_MANIFEST_DIR")).join(p))
        .find(|p| p.is_dir())
}

/// 临时项目（config_dir）——`project_root` 钳制要求 api 目录在它之内。
struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp_project() -> Tmp {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("oj-mail-e2e-proj-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    // canonicalize：loader 的 `ensure_within` 做双侧 canonical，未归一化的 macOS
    // `/var` ↔ `/private/var` 会让附件路径判定误报越界。
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d.canonicalize().unwrap())
}

/// 附件载荷（ASCII → lettre 7bit 原样送出，不进 base64）。
const ATT_PAYLOAD: &str = "OJ-MAIL-E2E-ATTACHMENT-PAYLOAD-42";
/// 上述载荷的 base64 形态（**必须不出现**在 `.eml` 里：附件字节不得被编码成文本流）。
const ATT_PAYLOAD_B64: &str = "T0otTUFJTC1FMkUtQVRUQUNITUVOVC1QQVlMT0FELTQy";

/// 项目夹具：config.yaml（`smtp.mock` = FileTransport）+ 一个 `mail` 模块
/// （`api.ts` 按 `?from=` 发信、`json.raw` 回原始信封）+ 附件文件。
fn write_project(t: &Tmp) -> PathBuf {
    let root = &t.0;
    let src = root.join("src");
    std::fs::create_dir_all(src.join("mail")).unwrap();
    let cfg = format!(
        concat!(
            "server:\n",
            "  host: \"127.0.0.1\"\n",
            "  port: 0\n",
            "  api_prefix: \"/v1/api\"\n",
            "db:\n",
            "  default: \"sqlite://{db}\"\n",
            "smtp:\n",
            "  workers: 1\n",
            "  queue_capacity: 4\n",
            "  mock:\n",
            "    host: \"localhost\"\n",
            "    port: 25\n",
            "    tls: none\n",
            "    allow_none_tls: true\n",
            "    mechanism: login\n",
            "    file_transport: \"{eml}\"\n",
            "    allowed_from: [\"noreply@x.com\"]\n",
            "    allowed_recipients: [\"@x.com\"]\n",
            // 严格清单模式：只装 oj-mail。**空对象值**不是透传——按「plugins: 统一语义」
            // 回落轴适配器，故插件 cfg 仍来自顶层 smtp: 段（用户无需在 plugins 里列全插件）。
            "plugins:\n",
            "  mail: {{}}\n",
        ),
        db = root.join("db.sqlite").display(),
        eml = eml_dir().display(),
    );
    std::fs::write(root.join("config.yaml"), cfg).unwrap();
    std::fs::write(root.join("attachment.txt"), ATT_PAYLOAD).unwrap();
    std::fs::write(
        src.join("mail/manifest.yaml"),
        "name: mail\ndesc: mail e2e\ndependencies: []\nversion: 0.1.0\n",
    )
    .unwrap();
    // `?from=` 可换发件人（负例用）；`?mode=enqueue` 走 enqueue 路（A1：统一信封 + 回查）；
    // `json.raw` 只回 mail 信封本身，断言无需拆两层。
    // profile key 走 `Mail("mock")`：钉住「键即 profile」的路由（与 smtp.mock 对应）。
    std::fs::write(
        src.join("mail/api.ts"),
        concat!(
            "export default {\n",
            "  async get() {\n",
            "    const from = String(http.query.from ?? \"noreply@x.com\");\n",
            "    const m = new Mail(\"mock\");\n",
            "    if (String(http.query.mode ?? \"\") === \"enqueue\") {\n",
            "      // 入队即回（统一信封，非裸 jobId）；随后用同队列的 send 作**屏障**：\n",
            "      // workers=1 + FIFO ⇒ send 的结果 resolve 时，enqueue 那封必然已完成并上送\n",
            "      // 宿主结果存储 ⇒ 下面的 result(jobId) 必命中（不依赖 sleep/时间竞态）。\n",
            "      const enq = await m.enqueue({\n",
            "        from, to: [\"a@x.com\"], subject: \"enq\", text: \"enqueued\",\n",
            "      });\n",
            "      const jobId = enq?.data?.jobId;\n",
            "      const barrier = await m.send({\n",
            "        from, to: [\"a@x.com\"], subject: \"barrier\", text: \"barrier\",\n",
            "      });\n",
            "      const res = await m.result(jobId);\n",
            "      json.raw({ enq, barrier, jobId, jobIdType: typeof jobId, res });\n",
            "      return;\n",
            "    }\n",
            "    const res = await m.send({\n",
            "      from,\n",
            "      to: [\"a@x.com\"],\n",
            "      subject: \"oj mail e2e\",\n",
            "      text: \"hello from e2e\",\n",
            "      attachments: [{ filename: \"note.txt\", path: \"attachment.txt\" }],\n",
            "    });\n",
            "    json.raw(res);\n",
            "  },\n",
            "};\n",
        ),
    )
    .unwrap();
    src
}

/// 真装配：读 config.yaml → 证书 → 插件目录 → `App::from_config`（dev 内省）。
/// 插件未归置时 `assemble_plugins` 会在装配期失败（文案指向 `cargo xtask plugin mail`）。
async fn boot(t: &Tmp, src: &Path) -> oj::app::App {
    let plugins = plugins_dir().unwrap_or_else(|| {
        panic!("bin/plugins 不存在：先跑 `cargo xtask plugin mail` 归置 oj-mail cdylib")
    });
    let mut cfg: Config = only_js::config::load_from(&t.0, Some("config.yaml")).unwrap();
    cfg.plugins_dir = Some(plugins);
    // 证书必配（无逃生口）：测试自带有效 JWS（有效期 [now-1h, now+1y]）。
    let n = server::test_support::now_secs();
    server::test_support::write_cert_into(
        &mut cfg.server,
        &t.0,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    oj::app::App::from_config(cfg, &t.0, src.to_path_buf(), "/v1/api".into(), true, false)
        .await
        .unwrap_or_else(|e| panic!("装配失败（先跑 `cargo xtask plugin mail`）：{e}"))
}

/// 进程内派发一个 GET（零 TCP：`App::dispatch` = router oneshot）。
async fn get(app: &oj::app::App, path: &str) -> (u16, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.dispatch(req).await;
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("响应不是 JSON（{e}）：{}", String::from_utf8_lossy(&bytes)));
    (status, v)
}

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

fn eml_count() -> usize {
    std::fs::read_dir(eml_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "eml"))
                .count()
        })
        .unwrap_or(0)
}
