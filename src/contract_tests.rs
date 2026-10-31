use super::*;
use serde_json::json;

fn build(
    params: Option<Value>,
    query: Option<Value>,
    body: Option<Value>,
) -> Result<(InputContract, HashMap<String, Regex>), String> {
    let mut pats = Vec::new();
    let c = InputContract::try_new(params, query, body, &mut pats)?;
    let mut compiled = HashMap::new();
    for p in pats {
        compiled.insert(p.clone(), Regex::new(&p).unwrap());
    }
    Ok((c, compiled))
}

fn ok(params: Option<Value>, query: Option<Value>, body: Option<Value>) -> InputContract {
    build(params, query, body).map(|(c, _)| c).unwrap()
}

fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn pats_of(c: &InputContract) -> HashMap<String, Regex> {
    // 经同一 ctor 重放 pattern 收集，保证与生产途经一致。
    let mut p = Vec::new();
    let _ = InputContract::try_new(
        c.params().cloned(),
        c.query().cloned(),
        c.body().cloned(),
        &mut p,
    );
    let mut out = HashMap::new();
    for s in p {
        out.insert(s.clone(), Regex::new(&s).unwrap());
    }
    out
}

// ---- 击穿：白名单外关键字必须 fail-fast，不得静默放行 ----

#[test]
fn unsupported_keyword_fails_fast_instead_of_silently_passing() {
    for kw in [
        "$ref",
        "oneOf",
        "allOf",
        "not",
        "format",
        "patternProperties",
        "dependencies",
    ] {
        let mut v = serde_json::Map::new();
        v.insert(kw.to_string(), Value::String("http://x".into()));
        let err =
            InputContract::try_new(None, None, Some(Value::Object(v)), &mut vec![]).unwrap_err();
        assert!(
            err.contains(kw) && err.contains("FAIL FAST"),
            "keyword `{kw}` must be rejected loudly, got: {err}"
        );
    }
}

#[test]
fn nested_unsupported_keyword_is_caught_too() {
    let body = json!({
        "type": "object",
        "properties": { "a": { "type": "string", "format": "email" } }
    });
    let err = InputContract::try_new(None, None, Some(body), &mut vec![]).unwrap_err();
    assert!(err.contains("format"), "嵌套关键字也必须被抓到: {err}");
}

// ---- 击穿：params/query 里永远无法满足的死契约必须拒绝 ----

#[test]
fn dead_contract_in_scalar_channels_is_rejected() {
    for ty in ["array", "object"] {
        let params = json!({
            "type": "object",
            "properties": { "tags": { "type": ty } }
        });
        let err = InputContract::try_new(Some(params), None, None, &mut vec![]).unwrap_err();
        assert!(
            err.contains("array") || err.contains("object"),
            "got: {err}"
        );
        assert!(
            err.contains("never") || err.contains("only scalar"),
            "got: {err}"
        );
    }
}

#[test]
fn scalar_channel_must_be_object() {
    let err = InputContract::try_new(Some(json!({ "type": "string" })), None, None, &mut vec![])
        .unwrap_err();
    assert!(err.contains("type must be \"object\""), "got: {err}");
}

#[test]
fn params_or_query_type_must_be_flat_scalars() {
    let err = InputContract::try_new(
            Some(json!({ "type": "object", "properties": { "id": {"type":"string","properties":{"x":{"type":"string"}}} } })),
            None, None, &mut vec![]).unwrap_err();
    assert!(err.contains("nested"), "got: {err}");
}

// ---- 类型与约束 ----

#[test]
fn valid_body_and_params_pass() {
    let body = json!({
        "type": "object",
        "required": ["name"],
        "additionalProperties": false,
        "properties": {
            "name": { "type": "string", "maxLength": 8 },
            "tags": { "type": "array", "minItems": 1, "items": { "type": "string" } },
            "addr": { "type": "object", "properties": { "city": { "type": "string" } } }
        }
    });
    let c = ok(None, None, Some(body));
    let good = json!({"name":"abc","tags":["a"],"addr":{"city":"hz"}});
    assert!(
        c.check(&map(&[]), &map(&[]), Some(&good), &pats_of(&c))
            .is_ok()
    );
    let empty = HashMap::new();
    assert!(c.check(&empty, &empty, Some(&good), &pats_of(&c)).is_ok());
}

#[test]
fn body_violations_are_reported() {
    let mk = || {
        ok(
            None,
            None,
            Some(json!({
                "type": "object",
                "required": ["n"],
                "additionalProperties": false,
                "properties": {
                    "n": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "s": { "type": "string", "minLength": 2, "maxLength": 4 }
                }
            })),
        )
    };
    let c = mk();
    let empty = HashMap::new();
    // 缺 required
    assert!(
        c.check(&empty, &empty, Some(&json!({})), &pats_of(&c))
            .unwrap_err()
            .contains("missing required field `n`")
    );
    // 类型不符
    assert!(
        c.check(&empty, &empty, Some(&json!({"n":"x"})), &pats_of(&c))
            .unwrap_err()
            .contains("expected integer")
    );
    // 越界
    assert!(
        c.check(&empty, &empty, Some(&json!({"n":99})), &pats_of(&c))
            .unwrap_err()
            .contains("<= 10")
    );
    assert!(
        c.check(&empty, &empty, Some(&json!({"n":0})), &pats_of(&c))
            .unwrap_err()
            .contains(">= 1")
    );
    // additionalProperties
    assert!(
        c.check(&empty, &empty, Some(&json!({"n":1,"x":2})), &pats_of(&c))
            .unwrap_err()
            .contains("additional property `x`")
    );
    // 嵌套字段路径
    assert!(
        c.check(
            &empty,
            &empty,
            Some(&json!({"n":1,"s":"abcdef"})),
            &pats_of(&c)
        )
        .unwrap_err()
        .contains("body.s")
    );
}

#[test]
fn body_must_be_json_when_body_contract_declared() {
    let c = ok(None, None, Some(json!({ "type": "object" })));
    let empty = HashMap::new();
    assert!(
        c.check(&empty, &empty, None, &pats_of(&c))
            .unwrap_err()
            .contains("expected object")
    );
}

// ---- 字符串强转（params/query 的核心语义） ----

#[test]
fn numeric_params_are_coerced_and_bad_values_rejected() {
    let c = ok(
        Some(json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "integer", "minimum": 1 } }
        })),
        None,
        None,
    );
    let empty = HashMap::new();
    assert!(
        c.check(&map(&[("id", "12")]), &empty, None, &pats_of(&c))
            .is_ok(),
        "数字字符串必须强转通过"
    );
    let err = c
        .check(&map(&[("id", "abc")]), &empty, None, &pats_of(&c))
        .unwrap_err();
    assert!(
        err.contains("id") && err.contains("expected integer"),
        "got: {err}"
    );
    let err = c
        .check(&map(&[("id", "0")]), &empty, None, &pats_of(&c))
        .unwrap_err();
    assert!(err.contains(">= 1"), "强转后仍受 minimum 约束: {err}");
    assert!(
        c.check(&empty, &empty, None, &pats_of(&c))
            .unwrap_err()
            .contains("required")
    );
}

#[test]
fn boolean_and_enum_in_query() {
    let c = ok(
        None,
        Some(json!({
            "type": "object",
            "properties": { "on": { "type": "boolean" }, "k": { "type": "string", "enum": ["a","b"] } }
        })),
        None,
    );
    let empty = HashMap::new();
    assert!(
        c.check(
            &empty,
            &map(&[("on", "true"), ("k", "a")]),
            None,
            &pats_of(&c)
        )
        .is_ok()
    );
    assert!(
        c.check(&empty, &map(&[("on", "yes")]), None, &pats_of(&c))
            .is_err(),
        "非 bool 字面量必须拒绝"
    );
    assert!(
        c.check(&empty, &map(&[("k", "c")]), None, &pats_of(&c))
            .unwrap_err()
            .contains("must be one of")
    );
}

#[test]
fn null_type_and_nullable() {
    let c = ok(
        Some(json!({ "type": "object", "properties": { "x": { "type": ["string","null"] } } })),
        None,
        None,
    );
    let empty = HashMap::new();
    assert!(
        c.check(&map(&[("x", "null")]), &empty, None, &pats_of(&c))
            .is_ok()
    );
    let c2 = ok(
        None,
        None,
        Some(json!({ "type": "object", "nullable": true, "additionalProperties": false })),
    );
    assert!(
        c2.check(&empty, &empty, Some(&Value::Null), &pats_of(&c2))
            .is_ok()
    );
}

// ---- pattern ----

#[test]
fn pattern_is_enforced_and_uses_rust_regex() {
    let body = json!({ "type": "object", "properties": { "tel": { "type": "string", "pattern": "^1[0-9]{10}$" } } });
    let c = ok(None, None, Some(body));
    let empty = HashMap::new();
    assert!(
        c.check(
            &empty,
            &empty,
            Some(&json!({"tel":"13800138000"})),
            &pats_of(&c)
        )
        .is_ok()
    );
    assert!(
        c.check(&empty, &empty, Some(&json!({"tel":"123"})), &pats_of(&c))
            .unwrap_err()
            .contains("does not match pattern")
    );
}

#[test]
fn invalid_pattern_fails_at_construction() {
    let body = json!({ "type": "object", "properties": { "a": { "type": "string", "pattern": "(?=x)" } } });
    let err = InputContract::try_new(None, None, Some(body), &mut vec![]).unwrap_err();
    assert!(
        err.contains("lookahead") || err.contains("not a valid Rust regex"),
        "got: {err}"
    );
}

#[test]
fn unknown_type_literal_fails_at_construction() {
    let err = InputContract::try_new(None, None, Some(json!({ "type": "str" })), &mut vec![])
        .unwrap_err();
    assert!(err.contains("unknown type"), "got: {err}");
}

#[test]
fn malformed_keyword_shape_fails_at_construction() {
    assert!(
        InputContract::try_new(None, None, Some(json!({ "required": "name" })), &mut vec![])
            .is_err()
    );
    assert!(
        InputContract::try_new(None, None, Some(json!({ "maxLength": "8" })), &mut vec![]).is_err()
    );
    assert!(InputContract::try_new(None, None, Some(json!({ "enum": [] })), &mut vec![]).is_err());
    assert!(
        InputContract::try_new(None, None, Some(json!({ "properties": "x" })), &mut vec![])
            .is_err()
    );
}

// ---- 击穿：query 数值 enum 的根因修复（Float(1.0) 必须命中 PosInt(1)） ----

#[test]
fn numeric_enum_in_query_matches_coerced_float() {
    // 复现 PR 根因：type 声明为 number、enum:[1,2]，前端传 type=1。
    // 强转得到 Float(1.0)，必须按数值命中 enum 成员，而非误报 got 1.0。
    let c = ok(
        None,
        Some(json!({
            "type": "object",
            "properties": { "type": { "type": "number", "enum": [1, 2] } }
        })),
        None,
    );
    let empty = HashMap::new();
    assert!(
        c.check(&empty, &map(&[("type", "1")]), None, &pats_of(&c))
            .is_ok(),
        "number + enum:[1,2] + type=1 必须放行"
    );
    // 整数字面量声明同样命中（PosInt vs PosInt）。
    let c2 = ok(
        None,
        Some(json!({
            "type": "object",
            "properties": { "type": { "type": "integer", "enum": [1, 2] } }
        })),
        None,
    );
    assert!(
        c2.check(&empty, &map(&[("type", "2")]), None, &pats_of(&c2))
            .is_ok()
    );
    // 非成员浮点值仍须被拒（1.5 不命中 1|2）。
    let err = c
        .check(&empty, &map(&[("type", "1.5")]), None, &pats_of(&c))
        .unwrap_err();
    assert!(err.contains("must be one of"), "got: {err}");
    // 越界整型仍须被拒。
    let err = c
        .check(&empty, &map(&[("type", "3")]), None, &pats_of(&c))
        .unwrap_err();
    assert!(err.contains("must be one of"), "got: {err}");
}

#[test]
fn default_keyword_allowed_but_unknown_still_fails() {
    // `default` 已加入 SUPPORTED_KEYWORDS：声明不再 FAIL FAST。
    assert!(
        InputContract::try_new(
            None,
            None,
            Some(json!({ "type": "string", "default": "x" })),
            &mut vec![]
        )
        .is_ok()
    );
    // 其它未知关键字仍 FAIL FAST（回归保护，静默跳过会让声明成谎话）。
    assert!(
        InputContract::try_new(
            None,
            None,
            Some(json!({ "type": "string", "format": "email" })),
            &mut vec![]
        )
        .is_err()
    );
}

// ---- v0.1.70：`response` 通道（只声明 + 审计，运行期不校验） ----

#[test]
fn audit_schema_rejects_unknown_channel() {
    // 击穿：拼错通道名以前**静默不生效**（接口裸奔），现在必须 fail-fast。
    for ch in ["param", "bodys", "responce", "querys", "header", "$schema"] {
        let err = audit_schema(&json!({ ch: { "type": "object" } })).unwrap_err();
        assert!(
            err.contains(ch) && err.contains("unsupported channel"),
            "`{ch}` 必须被通道白名单拒: {err}"
        );
    }
    // 合法四通道放行。
    assert!(
        audit_schema(&json!({
            "params": { "type": "object", "properties": { "id": { "type": "integer" } } },
            "query": { "type": "object", "properties": { "p": { "type": "string" } } },
            "body": { "type": "object" },
            "response": { "type": "object" }
        }))
        .is_ok()
    );
}

#[test]
fn response_channel_allows_nesting_unlike_scalar_channels() {
    // response 是 JSON 值：object / array / 嵌套都合法——不得被 params/query
    // 那套「仅扁平标量」规则误拒（那是入参来源是 HashMap<String,String> 决定的）。
    let resp = json!({
        "type": "object",
        "required": ["list"],
        "properties": {
            "list": { "type": "array", "items": {
                "type": "object", "properties": { "id": { "type": "integer" } }
            }},
            "total": { "type": "integer", "minimum": 0 }
        }
    });
    assert!(
        audit_schema(&json!({ "response": resp })).is_ok(),
        "response 允许嵌套 object/array"
    );
    // 反向击穿：同样的形态放在 params 里仍是死契约，必须被拒。
    let err = audit_schema(&json!({
        "params": { "type": "object", "properties": { "list": { "type": "array" } } }
    }))
    .unwrap_err();
    assert!(
        err.contains("only scalar") || err.contains("never"),
        "{err}"
    );
}

#[test]
fn response_channel_still_fails_fast_on_bad_declaration() {
    // 声明合法性照旧 fail-fast：未知关键字 / 非法类型 / 不可编译 pattern。
    let err = audit_schema(&json!({
            "response": { "type": "object", "properties": { "a": { "type": "string", "format": "email" } } }
        }))
        .unwrap_err();
    assert!(err.contains("format") && err.contains("FAIL FAST"), "{err}");

    let err = audit_schema(&json!({ "response": { "type": "str" } })).unwrap_err();
    assert!(err.contains("unknown type"), "{err}");

    let err = audit_schema(&json!({
            "response": { "type": "object", "properties": { "a": { "type": "string", "pattern": "(?=x)" } } }
        }))
        .unwrap_err();
    assert!(
        err.contains("lookahead") || err.contains("not a valid Rust regex"),
        "{err}"
    );
}

#[test]
fn dsl_boundary_is_exactly_the_documented_zod_subset() {
    // v0.1.71 对外承诺的边界（devkit「表达力边界」表）在此钉死：
    // 白名单内**全部**放行、白名单外**全部** fail-fast——没有第三种「静默降级」。
    let full = json!({
        "type": "object",
        "required": ["a"],
        "properties": {
            "a": { "type": ["string", "null"], "minLength": 1, "maxLength": 8,
                   "pattern": "^[a-z]+$", "enum": ["x", "y"], "default": "x" },
            "b": { "type": "integer", "minimum": 0, "maximum": 10, "nullable": true },
            "c": { "type": "array", "minItems": 1, "maxItems": 3,
                   "items": { "type": "object", "additionalProperties": false,
                              "properties": { "d": { "type": "boolean" } } } }
        },
        "additionalProperties": false
    });
    assert!(
        audit_schema(&json!({ "body": full })).is_ok(),
        "白名单全集必须放行"
    );

    // 不支持清单（含 zod 用户最常撞的几个）：一律 FAIL FAST，不静默忽略。
    for kw in [
        "$ref",
        "oneOf",
        "allOf",
        "anyOf",
        "not",
        "format",
        "patternProperties",
        "dependencies",
        "if",
        "then",
        "else",
        "const",
        "contains",
        "propertyNames",
        "uniqueItems",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "maxProperties",
        "minProperties",
    ] {
        let mut m = serde_json::Map::new();
        m.insert("type".to_string(), json!("object"));
        m.insert(kw.to_string(), json!("x"));
        let err = audit_schema(&json!({ "body": Value::Object(m) })).unwrap_err();
        assert!(
            err.contains(kw) && err.contains("FAIL FAST"),
            "`{kw}` 必须在边界清单外且 fail-fast: {err}"
        );
    }
}

#[test]
fn prototype_pollution_names_are_rejected_in_properties() {
    // openapi.json 是下游生成 TS/zod 的输入，`properties` 的键会原样变成生成源码
    // 里的键——这三个名字生成的字面量对象会改到原型上。
    for bad in ["__proto__", "constructor", "prototype"] {
        let mut props = serde_json::Map::new();
        props.insert(bad.to_string(), json!({ "type": "string" }));
        let err = audit_schema(&json!({ "body": { "type": "object", "properties": props } }))
            .unwrap_err();
        assert!(
            err.contains(bad) && err.contains("prototype-pollution"),
            "`{bad}` 必须被拒: {err}"
        );
    }
    // 正常字段名不受影响（含连字符、下划线、非 ASCII 都不禁）。
    assert!(
        audit_schema(&json!({
            "body": { "type": "object", "properties": { "a-b": { "type": "string" } } }
        }))
        .is_ok()
    );
}

#[test]
fn audit_schema_covers_input_channels_too() {
    // 唯一入口也要守住入参：openapi 生成 / build 落盘路径以前完全不审计，
    // 坏契约会原样进产物（下游现在拿 openapi.json 生成 client）。
    let err = audit_schema(&json!({ "body": { "type": "object", "oneOf": [] } })).unwrap_err();
    assert!(err.contains("oneOf") && err.contains("FAIL FAST"), "{err}");
    assert!(audit_schema(&json!({ "body": { "type": "object" } })).is_ok());
    // 非对象形态的 .schema 本身也不放行。
    assert!(audit_schema(&json!([])).is_err());
}

#[test]
fn empty_contract_validates_anything() {
    let c = InputContract::default();
    assert!(c.is_empty());
    let empty = HashMap::new();
    assert!(c.check(&empty, &empty, None, &HashMap::new()).is_ok());
}
