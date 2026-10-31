use super::*;

fn owner(user: &str) -> crate::bridge::handle_core::Owner {
    crate::bridge::handle_core::Owner {
        tenant: None,
        user: Some(user.into()),
        module: None,
        source: "test",
    }
}

fn section(global: usize, subject: usize) -> crate::bridge::db_policy::DbImportSection {
    crate::bridge::db_policy::DbImportSection {
        global_concurrency: global,
        per_subject_concurrency: subject,
        ..Default::default()
    }
}

#[test]
fn import_register_quota_global_and_subject() {
    // 全局满：global=2 两人各占一个，第三人撞 GlobalFull
    let reg = ImportRegistry::new(&section(2, 1));
    let (id1, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
    reg.register(owner("u2"), "a".into(), "t".into()).unwrap();
    let e = reg
        .register(owner("u3"), "a".into(), "t".into())
        .unwrap_err();
    assert_eq!(e, RegisterError::GlobalFull);
    assert!(e.message().contains("db.import: global"));
    // imp_ 前缀隔离
    assert!(id1.starts_with("imp_"));
    // 终态释放名额 → 可再登记
    reg.finish(&id1, Finish::Done { rows: 1, bytes: 2 });
    let (id3, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
    assert_ne!(id3, id1);
    // 每主体满：global 充裕、per_subject=1，同主体第二次撞 SubjectFull
    let reg = ImportRegistry::new(&section(8, 1));
    reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
    let e = reg
        .register(owner("u1"), "a".into(), "t".into())
        .unwrap_err();
    assert_eq!(e, RegisterError::SubjectFull);
    assert!(e.message().contains("per-subject"));
}

#[test]
fn import_status_cancel_sweep_roundtrip() {
    let reg = ImportRegistry::new(&section(4, 2));
    let (id, _) = reg
        .register(owner("u1"), "main".into(), "users".into())
        .unwrap();
    // status camelCase 逐键
    reg.progress(&id, 10, 2048);
    reg.push_sample(
        &id,
        ImportSample {
            line: 5,
            reason: "bad integer".into(),
            raw: Some("abc".into()),
        },
    );
    let v = reg.status(&id, &owner("u1")).unwrap();
    assert_eq!(v["taskId"], id);
    assert_eq!(v["state"], "running");
    assert_eq!(v["alias"], "main");
    assert_eq!(v["table"], "users");
    assert_eq!(v["rows"], 10);
    assert_eq!(v["bytes"], 2048);
    assert_eq!(v["errors"], 1);
    assert_eq!(v["samples"][0]["line"], 5);
    assert_eq!(v["samples"][0]["reason"], "bad integer");
    assert!(v["startedAtMs"].is_u64());
    assert!(v["finishedAtMs"].is_null());
    // 他人 id → None（不泄露存在性）
    assert!(reg.status(&id, &owner("u2")).is_none());
    // 前缀隔离：export 的 id 在 import 表 not found
    assert!(reg.status("exp_whatever", &owner("u1")).is_none());
    // cancel 幂等：首调 true，重调 false，终态 false，他人 None
    assert_eq!(reg.cancel(&id, &owner("u1")), Some(true));
    assert_eq!(reg.cancel(&id, &owner("u1")), Some(false));
    assert_eq!(reg.cancel(&id, &owner("u2")), None);
    assert_eq!(reg.cancel("imp_nonexistent", &owner("u1")), None);
    // finish Cancelled → 终态 + finishedAtMs
    reg.finish(
        &id,
        Finish::Cancelled {
            rows: 10,
            bytes: 2048,
        },
    );
    let v = reg.status(&id, &owner("u1")).unwrap();
    assert_eq!(v["state"], "cancelled");
    assert!(v["finishedAtMs"].is_u64());
    // 终态幂等：先到者赢（cancel 后再 finish Done 不翻盘）
    reg.finish(
        &id,
        Finish::Done {
            rows: 99,
            bytes: 99,
        },
    );
    assert_eq!(reg.status(&id, &owner("u1")).unwrap()["state"], "cancelled");
    // sweep：终态超时摘表；Running 豁免
    let now = ms_now();
    let removed = reg.sweep(now + 2 * 86_400_000, 86_400_000);
    assert!(removed.contains(&id));
    assert!(reg.status(&id, &owner("u1")).is_none());
    let (id2, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
    let removed = reg.sweep(now + 2 * 86_400_000, 86_400_000);
    assert!(!removed.contains(&id2));
}

#[test]
fn import_samples_capped_at_20_error_count_unbounded() {
    let reg = ImportRegistry::new(&section(4, 2));
    let (id, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
    for i in 0..30u64 {
        reg.push_sample(
            &id,
            ImportSample {
                line: i,
                reason: "r".into(),
                raw: None,
            },
        );
    }
    let v = reg.status(&id, &owner("u1")).unwrap();
    assert_eq!(v["samples"].as_array().unwrap().len(), 20);
    assert_eq!(v["errors"], 30); // error_count 独立计数，不受 samples 截断影响
    assert_eq!(v["samples"][19]["line"], 19); // 保留前 20（先到者）
    // 终态后 push_sample no-op
    reg.finish(
        &id,
        Finish::Failed {
            rows: 0,
            bytes: 0,
            error: "x".into(),
        },
    );
    reg.push_sample(
        &id,
        ImportSample {
            line: 99,
            reason: "late".into(),
            raw: None,
        },
    );
    let v = reg.status(&id, &owner("u1")).unwrap();
    assert_eq!(v["errors"], 30);
}
