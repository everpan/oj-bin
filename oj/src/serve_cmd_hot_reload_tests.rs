use super::{diff_configs, doc_sha};
use serde_yaml::Value;

/// 构造一个最小归一 raw 文档（各段给占位值，便于单点改写比对）。
fn base_doc() -> Value {
    serde_yaml::from_str(
        r#"
server: { host: "0.0.0.0", port: 9778 }
mounts:
  - { prefix: "/v1/api", api: "./src" }
db: { default: "sqlite://./a.db" }
redis: { default: "redis://localhost" }
vars: { FOO: "bar" }
"#,
    )
    .unwrap()
}

#[test]
fn diff_classifies_restart_pending() {
    let old = base_doc();
    let mut new = base_doc();
    // 改 db → 后端段：restart_pending（沿用旧池，重启后生效）。
    new["db"] = serde_yaml::from_str(r#"{ default: "sqlite://./b.db" }"#).unwrap();
    let (applied, restart_pending, kept) = diff_configs(&old, &new);
    assert!(restart_pending.contains(&"db".to_string()));
    assert!(!applied.contains(&"db".to_string()));
    assert!(!kept.contains(&"db".to_string()));
}

#[test]
fn diff_classifies_kept_listener() {
    let old = base_doc();
    let mut new = base_doc();
    // 改 server（port/host/证书）→ listener 级沿旧：kept_listener。
    new["server"] = serde_yaml::from_str(r#"{ host: "127.0.0.1", port: 9779 }"#).unwrap();
    let (_applied, restart_pending, kept) = diff_configs(&old, &new);
    assert!(kept.contains(&"server".to_string()));
    assert!(!restart_pending.contains(&"server".to_string()));
}

#[test]
fn diff_classifies_applied() {
    let old = base_doc();
    let mut new = base_doc();
    // 改 mounts（路由树）→ applied（本代生效；v0.1.122 D4 后 vars 等冻结段
    // 不再走 applied，见 diff_configs_frozen_sections_now_restart_pending）。
    new["mounts"] = serde_yaml::from_str(
        r#"
  - { prefix: "/v1/api", api: "./src" }
  - { prefix: "/v2/api", api: "./src2" }
"#,
    )
    .unwrap();
    let (applied, restart_pending, kept) = diff_configs(&old, &new);
    assert!(applied.contains(&"mounts".to_string()));
    assert!(!restart_pending.contains(&"mounts".to_string()));
    assert!(!kept.contains(&"mounts".to_string()));
}

/// 特权面段（ojm PR-3 安全阻塞项 3 修复，对抗单测）：platform / fs 段变更
/// 归 restart_pending——若随热更生效，特权模块可热改自己的 API/exec/fs
/// 白名单（自提权链）。HTTP 与 JS 回调共享此分类。
#[test]
fn diff_privilege_segments_never_hot_apply() {
    for key in ["platform", "fs"] {
        let old = base_doc();
        let mut new = base_doc();
        new[key] = serde_yaml::from_str(if key == "platform" {
            r#"{ modules: ["attacker"], root: "./p", trusted_paths: [] }"#
        } else {
            r#"{ root: "./elsewhere", readonly: false }"#
        })
        .unwrap();
        let (applied, restart_pending, kept) = diff_configs(&old, &new);
        assert!(
            restart_pending.contains(&key.to_string()),
            "{key} must be restart_pending: applied={applied:?} rp={restart_pending:?}"
        );
        assert!(
            !applied.contains(&key.to_string()),
            "{key} must not hot apply"
        );
        assert!(!kept.contains(&key.to_string()));
    }
}

#[test]
fn diff_unchanged_is_empty() {
    let old = base_doc();
    let new = base_doc();
    let (applied, restart_pending, kept) = diff_configs(&old, &new);
    assert!(applied.is_empty());
    assert!(restart_pending.is_empty());
    assert!(kept.is_empty());
}

#[test]
fn doc_sha_is_idempotent() {
    let a = base_doc();
    let b = base_doc();
    assert_eq!(doc_sha(&a).unwrap(), doc_sha(&b).unwrap());
}

#[test]
fn doc_sha_changes_on_edit() {
    let a = base_doc();
    let mut b = base_doc();
    b["vars"] = serde_yaml::from_str(r#"{ FOO: "changed" }"#).unwrap();
    assert_ne!(doc_sha(&a).unwrap(), doc_sha(&b).unwrap());
}

// ----- Task 4（v0.1.122）：锁检测 / applied 契约 / 冻结段归位 -----

use super::{lock_diff, locks_changed};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

fn lock(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(m, v)| (m.to_string(), v.to_string()))
        .collect()
}

/// B6 applied 契约：升级 = `-旧@v` + `+新@v`（先减后增）、新增 = `+`、卸载 = `-`，
/// 全量按模块名排序；无变化 = 空。
#[test]
fn lock_diff_add_remove_upgrade() {
    let old = lock(&[("a", "1.0.0"), ("c", "2.0.0")]);
    let new = lock(&[("a", "1.0.1"), ("b", "0.1.0")]);
    assert_eq!(
        lock_diff(&old, &new),
        vec![
            "-a@1.0.0".to_string(),
            "+a@1.0.1".to_string(),
            "+b@0.1.0".to_string(),
            "-c@2.0.0".to_string(),
        ]
    );
    assert!(lock_diff(&old, &old).is_empty());
}

/// B18 检测口径：同一条目跨挂载树移动（两锁文件互换）——merged map 无差但
/// per-file 有差，必须判「变」（否则装/卸空转）；文件键增删也算变。
#[test]
fn locks_changed_per_file_cross_mount_move() {
    let k1 = PathBuf::from("/d1/manifests.yaml");
    let k2 = PathBuf::from("/d2/manifests.yaml");
    let old = HashMap::from([(k1, lock(&[("a", "1.0.0")])), (k2, BTreeMap::new())]);
    let new = HashMap::from([
        (PathBuf::from("/d1/manifests.yaml"), BTreeMap::new()),
        (PathBuf::from("/d2/manifests.yaml"), lock(&[("a", "1.0.0")])),
    ]);
    assert!(locks_changed(&old, &new));
    assert!(!locks_changed(&old, &old));
    let shrunk = HashMap::from([(PathBuf::from("/d1/manifests.yaml"), lock(&[("a", "1.0.0")]))]);
    assert!(locks_changed(&old, &shrunk));
}

/// B19 后半：vars/oidc/db_query/db_trace/ws 冻结段改动 → restart_pending（不再误标 applied）。
#[test]
fn diff_configs_frozen_sections_now_restart_pending() {
    for key in ["vars", "oidc", "db_query", "db_trace", "ws"] {
        let old = base_doc();
        let mut new = base_doc();
        new[key] = serde_yaml::from_str("x: 1").unwrap();
        let (applied, restart_pending, _kept) = diff_configs(&old, &new);
        assert!(
            restart_pending.contains(&key.to_string()),
            "{key} must be restart_pending"
        );
        assert!(
            !applied.contains(&key.to_string()),
            "{key} must not hot apply"
        );
    }
}

// ----- Task 5（v0.1.122）：ojInfo capabilities 广播 -----

use super::{Registries, assemble_ojinfo};

/// ojInfo build 段报 reload capabilities（安装器/运维据此判定热装/匿名口径/
/// cron 存活能力可用与否）。
#[test]
fn ojinfo_has_capabilities() {
    let cfg = super::Config::default();
    let info = assemble_ojinfo(
        &cfg,
        &serde_json::json!({}),
        &[],
        &Registries::default(),
        None,
    );
    assert_eq!(
        info.build["capabilities"],
        serde_json::json!([
            "reload.assemble_modules",
            "reload.anon_per_reload",
            "reload.cron_survives",
        ])
    );
}
