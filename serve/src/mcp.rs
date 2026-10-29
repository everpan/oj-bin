//! OpenAPI 3.1 spec → MCP tool 列表（纯函数，零 I/O；MCP 双线路共享核心）。
//!
//! R3/R4（spec v2/v3）：MCP tool 名限定 `^[a-zA-Z0-9_-]{1,64}$`，而 openapi 的
//! operationId 是 `^[a-zA-Z0-9._~-]+$` 且无长度上限——本模块负责净化、净化后
//! 重跑唯一性门禁、并产出「client 可见 tool 对象 + 私有 dispatch 映射」的成对
//! 结果。两侧（serve 端点 / `oj mcp` codegen）消费同一份实现，不得各自重写。

use serde_json::{Value, json};

/// 单个 MCP tool：`tool` 是 client 可见的 JSON（name/description/inputSchema/
/// annotations/tags）；`dispatch` 是 MCP payload 不携带的分发映射
/// （method/oa_path 用于 tools/call 反查路由，operation_id 供 `/rpc` 使用）。
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub tool: Value,
    pub dispatch: ToolDispatch,
}

#[derive(Debug, Clone)]
pub struct ToolDispatch {
    pub operation_id: String,
    /// 标准 HTTP 方法小写（openapi 已把 oj 的 `del` 归一为 `delete`）。
    pub method: String,
    /// 带 base 前缀的 OpenAPI 路径（`{*x}` 已收敛为 `{x}`）。
    pub oa_path: String,
    /// catch-all（`{*name}`）参数名集合（spec 丢失该信息，由消费方按路由表回补；
    /// serve 端点不走它——raw_pattern_for 直接拿 raw 模板；`oj mcp` 模板据此决定
    /// 「原样插入 vs encodeURIComponent」）。空 = 无 catch-all 参数。
    pub catch_all: Vec<String>,
}

/// 净化 operationId 为合法 MCP tool 名：`[^a-zA-Z0-9_-]` → `_`；
/// 超 64 字符截断为 55 + "_" + 原串 fnv1a64 的 8 位十六进制（确定性）。
pub fn sanitize_name(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.len() > 64 {
        let h = fnv1a64(raw.as_bytes());
        s.truncate(55); // 55 + "_" + 8hex = 64 上限
        s.push_str(&format!("_{:08x}", h as u32));
    }
    s
}

/// fnv1a64：短输入的确定性散列（净化截断后缀与碰撞去重用，非密码学用途）。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// spec（`serve::openapi::generate` 产物）→ MCP tool 列表。
///
/// 拼装规则（spec「共享核心」节，逐字）：
/// - path/query parameters → inputSchema properties（`in:path` 恒入 required；
///   query 按 parameter.required）；requestBody schema → properties 挂 `body` 键，
///   其 required 数组并入顶层 required。
/// - 任意深度出现 `$ref` → Err（oj 生成物全内联；契约里出现 $ref 说明用户写了
///   工具解析不了的东西，宁可报错不可静默出残 schema）。
/// - `annotations`：GET → readOnlyHint；DELETE → destructiveHint；其他无该键。
/// - 未声明契约（无 parameters 且无 requestBody）→ description 追加
///   `" (no declared input contract — inspect source)"`（反目标 4 的诚实契约）。
/// - 名字唯一性门禁在**净化后**重跑：碰撞 → 对后出现的加 `_` + 8hex(原名) 后缀，
///   仍撞则对再后者继续，确定性。
pub fn spec_to_tools(spec: &Value) -> Result<Vec<McpTool>, String> {
    let paths = spec
        .get("paths")
        .and_then(|p| p.as_object())
        .ok_or("spec.paths missing or not an object")?;
    let mut out: Vec<McpTool> = Vec::new();
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (oa_path, path_obj) in paths {
        let Some(ops) = path_obj.as_object() else {
            continue;
        };
        for (method, op) in ops {
            let operation_id = op
                .get("operationId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("{method} {oa_path}: operationId missing"))?
                .to_string();
            let mut name = sanitize_name(&operation_id);
            // 净化后唯一性门禁（R4）：同名必须确定性消重，不得静默覆盖。
            if let Some(prev) = seen.get(&name)
                && prev != &operation_id
            {
                let h = fnv1a64(operation_id.as_bytes());
                let mut candidate = format!("{name}_{:08x}", h as u32);
                let mut n = 1u32;
                while seen.contains_key(&candidate) && seen[&candidate] != operation_id {
                    n += 1;
                    candidate = format!("{name}_{:08x}_{n}", h as u32);
                }
                name = candidate;
            }
            seen.insert(name.clone(), operation_id.clone());

            let mut description = op
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let (input_schema, declared) = build_input_schema(op, &name)?;
            if !declared {
                description.push_str(" (no declared input contract — inspect source)");
            }
            let mut tool = json!({
                "name": name,
                "description": description,
                "inputSchema": input_schema,
            });
            match method.as_str() {
                "get" => {
                    tool.as_object_mut()
                        .unwrap()
                        .insert("annotations".to_string(), json!({ "readOnlyHint": true }));
                }
                "delete" => {
                    tool.as_object_mut().unwrap().insert(
                        "annotations".to_string(),
                        json!({ "destructiveHint": true }),
                    );
                }
                _ => {}
            }
            if let Some(tags) = op.get("tags") {
                tool.as_object_mut()
                    .unwrap()
                    .insert("tags".to_string(), tags.clone());
            }
            out.push(McpTool {
                name,
                tool,
                dispatch: ToolDispatch {
                    operation_id,
                    method: method.clone(),
                    oa_path: oa_path.clone(),
                    catch_all: Vec::new(),
                },
            });
        }
    }
    Ok(out)
}

/// 由 operation 的 parameters/requestBody 拼 inputSchema；返回 (schema, 是否声明了契约)。
fn build_input_schema(op: &Value, tool_name: &str) -> Result<(Value, bool), String> {
    if op.to_string().contains("$ref") {
        return Err(format!(
            "tool `{tool_name}` inputSchema contains $ref — inline it in .schema"
        ));
    }
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();
    if let Some(params) = op.get("parameters").and_then(|p| p.as_array()) {
        for p in params {
            let pname = p
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let schema = p
                .get("schema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "string" }));
            properties.insert(pname.clone(), schema);
            let is_path = p.get("in").and_then(|v| v.as_str()) == Some("path");
            let is_required = p.get("required").and_then(|v| v.as_bool()) == Some(true);
            if (is_path || is_required) && !required.contains(&pname) {
                required.push(pname);
            }
        }
    }
    let mut declared = !properties.is_empty();
    if let Some(body) = op
        .get("requestBody")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get("application/json"))
        .and_then(|j| j.get("schema"))
    {
        declared = true;
        properties.insert("body".to_string(), body.clone());
        // body 自身有 required 字段 → `body` 键整体必填（子 schema 的 required 原样内嵌）。
        let body_has_required = body
            .get("required")
            .and_then(|r| r.as_array())
            .is_some_and(|a| !a.is_empty());
        if body_has_required && !required.contains(&"body".to_string()) {
            required.push("body".to_string());
        }
    }
    let mut schema = json!({ "type": "object", "properties": Value::Object(properties) });
    if !required.is_empty() {
        schema
            .as_object_mut()
            .unwrap()
            .insert("required".to_string(), json!(required));
    }
    Ok((schema, declared))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼 spec（与 generate 产物同形），让本模块单测不依赖路由表/V8。
    fn spec(ops: &[(&str, &str, Value)]) -> Value {
        let mut paths = serde_json::Map::new();
        for (path, method, op) in ops {
            paths
                .entry(path.to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert(method.to_string(), op.clone());
        }
        json!({ "openapi": "3.1.0", "paths": Value::Object(paths) })
    }

    fn op(operation_id: &str, extra: Value) -> Value {
        let mut o = json!({ "operationId": operation_id, "summary": format!("OP {operation_id}") });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                o.as_object_mut().unwrap().insert(k.clone(), v.clone());
            }
        }
        o
    }

    // ---------- Task 2：名字净化与净化后唯一性 ----------

    // Given 两个净化后同名的 operationId（"a.b_get" 与 "a_b_get"）
    // When spec_to_tools
    // Then 后者确定性加 hash 后缀消重，且两次运行结果一致
    #[test]
    fn sanitized_name_collisions_are_deduped_deterministically() {
        let s = spec(&[
            ("/a", "get", op("a.b_get", json!({}))),
            ("/b", "get", op("a_b_get", json!({}))),
        ]);
        let t1 = spec_to_tools(&s).unwrap();
        let t2 = spec_to_tools(&s).unwrap();
        assert_eq!(t1.len(), 2);
        assert_ne!(t1[0].name, t1[1].name, "净化后同名必须消重");
        let n1: Vec<String> = t1.iter().map(|t| t.name.clone()).collect();
        let n2: Vec<String> = t2.iter().map(|t| t.name.clone()).collect();
        assert_eq!(n1, n2, "消重必须是确定性的");
        // dispatch 仍指回原始 operationId（名字≠operationId 时映射不得丢）。
        let ids: Vec<&str> = t1
            .iter()
            .map(|t| t.dispatch.operation_id.as_str())
            .collect();
        assert!(ids.contains(&"a.b_get") && ids.contains(&"a_b_get"));
    }

    // Given >64 字符的 operationId
    // When sanitize_name
    // Then ≤64、以 "_" + 8hex 结尾、同输入同输出、不同输入后缀不同
    #[test]
    fn long_names_are_truncated_with_hash_suffix() {
        let long = format!("{}_get", "seg_".repeat(20)); // 124 字符
        let n1 = sanitize_name(&long);
        assert!(n1.len() <= 64, "超长名必须截断: {n1}");
        assert_eq!(n1, sanitize_name(&long), "同输入必须同输出");
        let n2 = sanitize_name(&format!("{long}x"));
        assert_ne!(n1, n2, "不同输入后缀必须不同");
        // 合法字符集
        assert!(
            n1.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "净化结果必须落在 MCP 合法字符集: {n1}"
        );
    }

    // ---------- Task 3：inputSchema 拼装 / $ref / annotations / 描述标注 ----------

    // Given 契约齐备的 POST（path id:integer + query page:required + body name:required）
    // When spec_to_tools
    // Then required 含 id/page/body，properties 含 body 且 id type integer，body 子 schema 的 required 原样内嵌
    #[test]
    fn contract_route_builds_merged_input_schema() {
        let s = spec(&[(
            "/v1/api/user/{id}",
            "post",
            op(
                "user__id__post",
                json!({
                    "parameters": [
                        { "name": "id", "in": "path", "required": true,
                          "schema": { "type": "integer", "minimum": 1 } },
                        { "name": "page", "in": "query", "required": true,
                          "schema": { "type": "integer" } },
                        { "name": "kw", "in": "query", "required": false,
                          "schema": { "type": "string" } }
                    ],
                    "requestBody": { "content": { "application/json": { "schema": {
                        "type": "object", "required": ["name"],
                        "properties": { "name": { "type": "string" } }
                    } } } }
                }),
            ),
        )]);
        let tools = spec_to_tools(&s).unwrap();
        let schema = &tools[0].tool["inputSchema"];
        let req: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(req.contains(&"id") && req.contains(&"page") && req.contains(&"body"));
        assert!(!req.contains(&"kw"), "可选 query 不得入 required");
        assert_eq!(schema["properties"]["id"]["type"], "integer");
        assert_eq!(schema["properties"]["body"]["required"][0], "name");
    }

    // Given 契约里任意深度嵌 $ref
    // When spec_to_tools
    // Then Err 且文案含工具名与 "$ref"
    #[test]
    fn ref_anywhere_fails_fast() {
        let s = spec(&[(
            "/x",
            "post",
            op(
                "x_post",
                json!({
                    "requestBody": { "content": { "application/json": { "schema": {
                        "type": "object",
                        "properties": { "nested": { "$ref": "#/components/schemas/Y" } }
                    } } } }
                }),
            ),
        )]);
        let err = spec_to_tools(&s).unwrap_err();
        assert!(err.contains("x_post") && err.contains("$ref"), "{err}");
    }

    // Given GET 与 DELETE 端点
    // Then 前者 annotations.readOnlyHint==true，后者 destructiveHint==true，POST 无 annotations 键
    #[test]
    fn method_annotations_follow_http_verb() {
        let s = spec(&[
            ("/a", "get", op("a_get", json!({}))),
            ("/b", "delete", op("b_delete", json!({}))),
            ("/c", "post", op("c_post", json!({}))),
        ]);
        let tools = spec_to_tools(&s).unwrap();
        let by_name = |n: &str| tools.iter().find(|t| t.name == n).unwrap();
        assert_eq!(by_name("a_get").tool["annotations"]["readOnlyHint"], true);
        assert_eq!(
            by_name("b_delete").tool["annotations"]["destructiveHint"],
            true
        );
        assert!(
            by_name("c_post").tool.get("annotations").is_none(),
            "POST 不得有 annotations"
        );
    }

    // Given 无契约端点（无 parameters 且无 requestBody）
    // Then description 以 "(no declared input contract — inspect source)" 结尾
    #[test]
    fn undeclared_contract_is_flagged_in_description() {
        let s = spec(&[("/a", "get", op("a_get", json!({})))]);
        let tools = spec_to_tools(&s).unwrap();
        let d = tools[0].tool["description"].as_str().unwrap();
        assert!(
            d.ends_with("(no declared input contract — inspect source)"),
            "{d}"
        );
    }

    // Given delete 动词端点（openapi 已归一）
    // Then dispatch.method == "delete"，dispatch.oa_path 与 spec 路径一致
    #[test]
    fn dispatch_method_uses_normalized_delete() {
        let s = spec(&[(
            "/v1/api/user/{id}",
            "delete",
            op("user__id__delete", json!({})),
        )]);
        let tools = spec_to_tools(&s).unwrap();
        assert_eq!(tools[0].dispatch.method, "delete");
        assert_eq!(tools[0].dispatch.oa_path, "/v1/api/user/{id}");
        assert_eq!(tools[0].dispatch.operation_id, "user__id__delete");
    }

    // Given 带 tags 的 operation
    // Then tool 对象保留 tags（modules 白名单过滤的消费面）
    #[test]
    fn tags_are_retained_for_module_filtering() {
        let mut o = op("a_get", json!({}));
        o.as_object_mut()
            .unwrap()
            .insert("tags".into(), json!(["user"]));
        let s = spec(&[("/a", "get", o)]);
        let tools = spec_to_tools(&s).unwrap();
        assert_eq!(tools[0].tool["tags"][0], "user");
    }
}
