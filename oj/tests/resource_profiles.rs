//! 资源根 key 多源选择（v0.1.34）端到端场景覆盖。
//!
//! 经 `assemble_backend` 验证 `ResourceProfiles` 在装配期把选中 profile 烘焙为默认源、
//! 或选错时 fail-fast。db 走内置 sqlite（无需插件），故本文件不依赖任何 cdylib 插件，
//! 可在无 `bin/plugins` 的环境稳定跑。其余轴（redis/es/broker/kafka/rabbit）的选源与
//! fail-fast 逻辑由 `oj/src/{app,serve_cmd}.rs` 的单元测试覆盖（不依赖插件即可触发
//! fail-fast 分支）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use oj::app::{ResourceProfiles, assemble_backend};
use oj_plugin_ffi::path_util::sqlite_file_dsn;
use only_js::config::Config;

fn tmp() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!(
        "oj-rp-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    t
}

/// `oj test/exec --db <profile>`：选中 profile 经 `db_override` 烘焙为默认库；
/// 未声明的 profile → fail-fast（不静默回落 default）。
#[tokio::test]
async fn db_profile_selection_redirects_default_and_fails_fast_on_missing() {
    let dir = tmp();
    let dsn_default = sqlite_file_dsn(&dir.join("default.db"));
    let dsn_report = sqlite_file_dsn(&dir.join("report.db"));
    let mut cfg = Config::default();
    cfg.db.insert("default".into(), dsn_default);
    cfg.db.insert("report".into(), dsn_report);

    // 选中 report → 装配期把 default 重定向落到 report（oj test --db report 的形态）。
    let profiles = ResourceProfiles {
        db: Some("report".into()),
        ..Default::default()
    };
    let backend = assemble_backend(
        &cfg,
        &serde_json::Value::Null,
        &dir,
        &[(dir.clone(), true)],
        "/v1/api",
        true,
        &profiles,
    )
    .await
    .expect("assemble with valid --db report");
    assert_eq!(
        backend.stable().db_override.as_deref(),
        Some("report"),
        "default db must be redirected to the selected profile"
    );

    // 未声明的 profile → fail-fast（不静默回落 default，防误用开发库）。
    let profiles = ResourceProfiles {
        db: Some("ghost".into()),
        ..Default::default()
    };
    let e = assemble_backend(
        &cfg,
        &serde_json::Value::Null,
        &dir,
        &[(dir.clone(), true)],
        "/v1/api",
        true,
        &profiles,
    )
    .await
    .err()
    .unwrap_or_default();
    assert!(
        e.contains("--db"),
        "missing db profile must fail-fast, got: {e}"
    );
}

/// 多库全 default：未传 `--db` → 保持字面 default（db_override 为空，运行期按同名查找）。
#[tokio::test]
async fn db_profile_default_when_no_override() {
    let dir = tmp();
    let dsn_default = sqlite_file_dsn(&dir.join("default.db"));
    let dsn_report = sqlite_file_dsn(&dir.join("report.db"));
    let mut cfg = Config::default();
    cfg.db.insert("default".into(), dsn_default);
    cfg.db.insert("report".into(), dsn_report);

    let backend = assemble_backend(
        &cfg,
        &serde_json::Value::Null,
        &dir,
        &[(dir.clone(), true)],
        "/v1/api",
        true,
        &ResourceProfiles::default(),
    )
    .await
    .expect("assemble with no --db");
    assert!(
        backend.stable().db_override.is_none(),
        "no --db means default stays literal default"
    );
}
