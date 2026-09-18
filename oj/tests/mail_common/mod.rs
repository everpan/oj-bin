//! mail 端到端测试的**共享夹具**：真装配（`smtp:` 段 + `oj-mail` cdylib + `App::from_config`）
//! → HTTP 路由里的 JS `mail.*` → 真 `.eml` 落盘。
//!
//! 由 `mail_e2e.rs`（投递/白名单/enqueue 契约）与 `mail_drain_e2e.rs`（停机排空 A4）共用。
//! **两个目标各自一个进程**：`oj-mail` 的引擎是插件内**进程级单例**（首次 init 钉死
//! transport 与 `file_transport` 目录），且 `drain` 会永久停掉该单例——共进程会互相污染。
//!
//! 前置：`cargo xtask plugin mail`（cdylib 归置到 `bin/plugins/<host-triple>/`）。
//!
//! 覆盖链：config.yaml 解析（`SmtpSection`）→ `plugin_cfg("mail")` 适配器 → 插件 init
//! （建 transport）→ `build_mail_backend`（宿主白名单面）→ `Extras.mail`/`StableState.mail`
//! → 路由 + 真 runtime 的 JS 全局 `mail` → vtable `submit` → 引擎有界队列 + worker
//! → lettre `FileTransport` 写 `.eml`。全程不依赖网络（零 SMTP 连接）。

// 夹具按需取用：单个目标不会用到全部帮助函数（共享模块的常态）。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use axum::body::Body;
use axum::http::Request;
use only_js::config::Config;

/// 目标内用例串行化（共享落盘目录与进程级插件引擎）。
pub fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 落盘目录（**全目标共用**）：插件引擎只认首次 init 的 `file_transport`，故各用例的
/// `smtp:` 段必须逐字相同（同一目录）——重复 init 由插件幂等吸收。
pub fn eml_dir() -> &'static PathBuf {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("oj-mail-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
}

/// `<workspace_root>/bin/plugins`（xtask 归置目录）：两候选谁存在用谁；都不存在留 None
/// → 装配期报「找不到插件清单」并指向 `cargo xtask plugin mail`。
pub fn plugins_dir() -> Option<PathBuf> {
    ["../bin/plugins", "../../../../bin/plugins"]
        .iter()
        .map(|p| Path::new(env!("CARGO_MANIFEST_DIR")).join(p))
        .find(|p| p.is_dir())
}

/// 临时项目（config_dir）——`project_root` 钳制要求 api 目录在它之内。
pub struct Tmp(pub PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// YAML 双引号标量里的**纯路径**字段（`file_transport`）：Windows 反斜杠在 YAML 里
/// 是转义序列（`C:\Users` 的 `\U` → unknown escape），故转正斜杠；Windows 文件 API
/// 接受正斜杠，该字段随后被当路径直接使用（不再经 DSN 解析），故裸 replace 即可。
///
/// **不要**用它处理 `sqlite://` DSN：`tmp_project()` 的 `canonicalize()` 在 Windows
/// 上产出 verbatim 前缀 `\\?\D:\...`，裸 replace 会把 `\\?\` 变成 `//?/`，从而命中
/// `normalize_sqlite_dsn` 的「`//` 直通」分支（db_backend.rs），跳过盘符修正 →
/// SQLITE_CANTOPEN。DSN 一律走 `oj_plugin_ffi::path_util::sqlite_file_dsn`（它先经
/// `dunce` 剥 verbatim 再转正斜杠 + 用单冒号）。
fn fwd(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

pub fn tmp_project() -> Tmp {
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
pub const ATT_PAYLOAD: &str = "OJ-MAIL-E2E-ATTACHMENT-PAYLOAD-42";
/// 上述载荷的 base64 形态（**必须不出现**在 `.eml` 里：附件字节不得被编码成文本流）。
pub const ATT_PAYLOAD_B64: &str = "T0otTUFJTC1FMkUtQVRUQUNITUVOVC1QQVlMT0FELTQy";

/// 项目夹具：config.yaml（`smtp.mock` = FileTransport）+ 一个 `mail` 模块
/// （`api.ts`：默认 `?from=` 发信；`?mode=enqueue` 走 enqueue 路）+ 附件文件。
pub fn write_project(t: &Tmp) -> PathBuf {
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
            "  default: \"{db}\"\n",
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
        db = oj_plugin_ffi::path_util::sqlite_file_dsn(&root.join("db.sqlite")),
        eml = fwd(eml_dir()),
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
pub async fn boot(t: &Tmp, src: &Path) -> oj::app::App {
    try_boot(t, src)
        .await
        .unwrap_or_else(|e| panic!("装配失败（先跑 `cargo xtask plugin mail`）：{e}"))
}

/// 同 [`boot`]，但把装配错误**返回**（配置负例用：装配期闸门必须 fail-fast）。
pub async fn try_boot(t: &Tmp, src: &Path) -> Result<oj::app::App, String> {
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
    oj::app::App::from_config(
        cfg,
        &t.0,
        src.to_path_buf(),
        "/v1/api".into(),
        true,
        false,
        None,
    )
    .await
}

/// 进程内派发一个 GET（零 TCP：`App::dispatch` = router oneshot）。
pub async fn get(app: &oj::app::App, path: &str) -> (u16, serde_json::Value) {
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

/// 落盘 `.eml` 数（白名单负例断言「未落盘」用）。
pub fn eml_count() -> usize {
    std::fs::read_dir(eml_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "eml"))
                .count()
        })
        .unwrap_or(0)
}
