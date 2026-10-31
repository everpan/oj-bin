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
        // 改 vars（部署期常量，无需重建后端）→ applied（本代生效）。
        new["vars"] = serde_yaml::from_str(r#"{ FOO: "baz" }"#).unwrap();
        let (applied, restart_pending, kept) = diff_configs(&old, &new);
        assert!(applied.contains(&"vars".to_string()));
        assert!(!restart_pending.contains(&"vars".to_string()));
        assert!(!kept.contains(&"vars".to_string()));
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
