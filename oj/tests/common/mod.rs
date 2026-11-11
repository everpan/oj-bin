//! e2e 共享 boot 夹具：e2e.rs 与 e2e_svc.rs 两个测试二进制各自编译本模块
//! （cargo 测试目标间无运行时共享，纯源码复用）。
//!
//! 为什么拆两个二进制：插件是**每进程单例**（oj-auth GUARD 等 OnceLock 首次
//! init 后忽略后续 cfg，见 e2e.rs:1039 子进程形态注释）——auth 配置不同的
//! boot 不得同进程混跑。e2e.rs 全程 cfg.auth=None；需要真实守卫链（svc
//! scopes/obo 换签）的用例住 e2e_svc.rs，独立进程独立 init。

// lock() 的 std MutexGuard 有意全程持有（e2e 全用例串行），横跨 await 是设计而非疏漏。
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use oj::serve_cmd;
use only_js::config::Config;

pub fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// YAML 双引号标量里的**纯路径**字段：Windows 反斜杠在 YAML 里是转义序列
/// （`C:\Users` 的 `\U` → unknown escape），故转正斜杠；Windows 文件 API 接受正斜杠。
///
/// **不要**用它处理 `sqlite://` DSN：`canonicalize()` 在 Windows 上产出 verbatim 前缀
/// `\\?\D:\...`，裸 replace 会把 `\\?\` 变成 `//?/`，从而命中 `normalize_sqlite_dsn`
/// 的「`//` 直通」分支（src/bridge/db_backend.rs），跳过盘符修正 → SQLITE_CANTOPEN。
/// DSN 一律走 `oj_plugin_ffi::path_util::sqlite_file_dsn`（先 `dunce` 剥 verbatim，
/// 再转正斜杠，并用单冒号 `sqlite:`）。
// 二进制间按需取用：e2e.rs 用全套，e2e_svc.rs 只用 boot_inner/lock。
#[allow(dead_code)]
pub fn fwd(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

pub fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sample")
        .canonicalize()
        .unwrap()
}

#[allow(dead_code)]
pub async fn boot(dev: bool) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, PathBuf) {
    boot_inner(dev, |_| {}).await.unwrap()
}

/// boot 的可调变体：tweak 在装配前修改 config（MCP/RPC 端点门控等用例用）。
/// 返回 Result 供「装配期 fail-fast」用例直接断言 Err。
pub async fn boot_inner(
    dev: bool,
    tweak: impl FnOnce(&mut Config),
) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>, PathBuf), String> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // config_dir = sample 项目根（loader 的 project_root 钳制要求 api 在根内）；
    // 仅 db 用独立临时文件隔离，seed 由 start() 对新库重放。
    let tmp = std::env::temp_dir().join(format!("oj-e2e-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let root = sample();
    let mut cfg: Config =
        serde_yaml::from_str(&std::fs::read_to_string(root.join("config.yaml")).unwrap()).unwrap();
    cfg.server.port = 0;
    cfg.db.insert(
        "default".into(),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db.sqlite")),
    );
    // e2e 是 v0.1 UC 验收（不带租户头/不登录）；sample 的 tenant/auth 留给手工冒烟，
    // 租户注入/400 与鉴权全链路在 mdm-server::tests 覆盖。mcp 同理清掉：sample
    // config 带 mcp.enabled 示范段，而本夹具 auth=None → enabled 无 auth 拒启
    // fail-fast；需要 mcp/rpc 的用例（e2e_svc）在 tweak 里显式开。
    cfg.tenant = Default::default();
    cfg.auth = None;
    cfg.server.mcp = None;
    // sample/config.yaml 的 web 挂载指向 dist（gitignore，CI 新克隆无此目录）且
    // api 挂载固定 src——boot() 按本用例的 dev/release 目录重挂：清掉 sample 挂载，
    // 由 start() 注入 (dir, /v1/api) 一条（UC 不覆盖静态兜底，多站点静态有专属 e2e，
    // 见 multi_static_sites_serve_by_longest_prefix_end_to_end）。
    cfg.mounts.clear();
    // 平台特权挂载（v0.1.75 ojm 批次 D4 起 sample/config.yaml 带 platform 段）：
    // platform.root 须经 mounts[].api 接入，否则启动即 fail-fast。boot 清掉 sample
    // 挂载后必须补回 /_platform → platform-root（源码目录，dev/release 双模式都以
    // ts 挂载服务——mixed mounts 逐挂载判模式）。
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/_platform".into(),
        api: Some(
            root.join("platform-root")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        ),
        web: None,
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    tweak(&mut cfg);
    // 证书必配（无逃生口）：启动需真实签名证书，随测试临时目录生成（有效期 1 年）。
    let n = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        &tmp,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    let dir = if dev {
        root.join("src")
    } else {
        // release：现场构建 sample → 项目根内临时 dist（loader 的 project_root 钳制
        // 要求 dist ⊆ config_dir；sample/dist 旧格式已废弃，重生成为 T9 交付）。
        let dist = root.join(".e2e-dist");
        let _ = std::fs::remove_dir_all(&dist);
        oj::build_cmd::run(&oj::args::BuildArgs {
            module: None,
            config: Some("config.yaml".into()),
            dir: root.join("src").display().to_string(),
            module_root: None,
            out: dist.display().to_string(),
            minify: true,
            check: false,
            bundle: false,
        })
        .await
        .unwrap();
        dist
    };
    // release 语义（README 部署故事）：build 后先 `oj migrate` 再 server——
    // release verify 门禁要求账本与产物齐平，空库首启必须先显式迁移。
    if !dev {
        let acc = only_js::bridge::DbBackendRegistry::builtin()
            .connect(cfg.db.get("default").unwrap(), &root)
            .await
            .unwrap();
        oj::migrate::apply_all(Some(&acc), &dir, false, false, None)
            .await
            .unwrap();
    }
    serve_cmd::start(cfg, &root, dir, "/v1/api".into())
        .await
        .map_err(|e| e.to_string())
        .map(|(addr, h)| (addr, h, tmp))
}
