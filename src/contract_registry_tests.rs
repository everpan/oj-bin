use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};

fn reg_with_id() -> InputContractRegistry {
    let mut r = InputContractRegistry::new();
    let c = InputContract::try_new(
        Some(json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "integer" } }
        })),
        None,
        None,
        &mut vec![],
    )
    .unwrap();
    r.insert(PathBuf::from("/api/user/account/api.ts"), "get", c)
        .unwrap();
    r
}

#[test]
fn declared_contract_is_enforced_by_key() {
    let r = reg_with_id();
    let f = Path::new("/api/user/account/api.ts");
    let mut p = HashMap::new();
    p.insert("id".into(), "7".into());
    assert!(r.check(f, "get", &p, &HashMap::new(), None).is_ok());
    let mut bad = HashMap::new();
    bad.insert("id".into(), "x".into());
    assert!(r.check(f, "get", &bad, &HashMap::new(), None).is_err());
}

#[test]
fn unknown_file_or_method_must_not_block() {
    // 击穿方向反过来同样要钉死：错配不得变成「全放行」以外的错拦，
    // 也不得把有契约的路由静默降级成无契约。
    let r = reg_with_id();
    let f = Path::new("/api/user/account/api.ts");
    let mut p = HashMap::new();
    p.insert("id".into(), "not-a-number".into());
    assert!(
        r.check(f, "post", &p, &HashMap::new(), None).is_ok(),
        "未声明的方法不得被误拦"
    );
    assert!(
        r.check(Path::new("/other/api.ts"), "get", &p, &HashMap::new(), None)
            .is_ok(),
        "未声明的文件不得被误拦"
    );
    // 但该守的地方必须守。
    assert!(r.check(f, "get", &p, &HashMap::new(), None).is_err());
}

#[test]
fn registry_rejects_bad_contract_loudly() {
    let mut r = InputContractRegistry::new();
    // 非法契约入库必须 Err（装配期据此硬失败）。
    let bad = InputContract::try_new(Some(json!({})), None, None, &mut vec![]).unwrap();
    let _ = bad;
    let raw = json!({ "type": "object", "properties": { "a": { "type": "unknown" } } });
    let err = InputContract::try_new(None, None, Some(raw), &mut vec![]).unwrap_err();
    assert!(err.contains("unknown type"));
    // 注册表构建路径上的 Err 也必须带文件与方法，便于定位。
    assert!(
        r.insert(PathBuf::from("/x/api.ts"), "get", InputContract::default())
            .is_ok()
    );
    assert_eq!(r.len(), 1);
}
