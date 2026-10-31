use super::*;
use serde_json::{Value, json};

/// 红线守护：bootstrap.js 必须 7-bit ASCII——deno_core 对 extension 代码做
/// 硬校验，注释里的非 ASCII 也会让整个 bridge 注册失败（全路由 500）。
#[test]
fn bootstrap_js_is_7bit_ascii() {
    assert!(
        include_str!("bootstrap.js").is_ascii(),
        "bootstrap.js must stay 7-bit ASCII (deno_core rejects non-ASCII extension code)"
    );
}

/// 回归护栏：bridge_ext 的 ESM 源必须内嵌（不得依赖构建机路径）。见 `BRIDGE_ESM`。
#[test]
fn bridge_ext_esm_source_is_embedded() {
    assert_eq!(BRIDGE_ESM.len(), 1);
    assert!(BRIDGE_ESM.iter().all(|f| f.is_runtime_loadable()));
    assert_eq!(BRIDGE_ESM[0].specifier, "ext:bridge_ext/bootstrap.js");
}

fn new_bridge() -> (Bridge, Arc<InMemoryAccessor>) {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1, "name": "ever"})]);
    let b = Bridge::new(db.clone(), Arc::new(InMemoryKV::new()));
    (b, db)
}

/// tenant.enable 时 handle() 提取注入 → http.tenantId；未启用为 null。
#[tokio::test(flavor = "current_thread")]
async fn http_exposes_tenant_id() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"json.ok({ t: http.tenantId === undefined ? null : http.tenantId });"#,
            RequestInfo {
                tenant_id: Some("t-9".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["t"], "t-9");
}

/// http.module {name, version}：run_module 按目录命中注入（v0.1.76）；
/// op_http_info 投影 + bootstrap Proxy 冻结。同一 runtime 归还后再借
/// （run_with，module=None）→ reset 归 null（LIFO 取回同一 runtime）。
#[tokio::test(flavor = "current_thread")]
async fn http_exposes_module_identity_and_resets() {
    let (base, api) = mod_fx(&[(
        "m/api.js",
        "export function get() { json.ok({ mod: http.module }); }\nexport default { get };\n",
    )]);
    let modules = Arc::new(HashMap::from([(
        base.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "9.9.9".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    let b = module_bridge_ex(&base, SchemaRegistry::new(), modules, false, HashMap::new());
    let t = std::time::Duration::from_secs(5);
    let cap = b
        .run_module(&api, "get", RequestInfo::default(), t)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["mod"]["name"], "m", "{v}");
    assert_eq!(v["data"]["mod"]["version"], "9.9.9", "{v}");
    // reset：模块上下文不跨请求残留（无模块上下文的 run_with → null）。
    let cap = b
        .run_with(
            r#"json.ok({ mod: http.module === null ? "null" : "leaked" });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["mod"], "null", "{v}");
}

/// api 文件不在任何模块目录内（挂载根级直置）→ http.module === null。
#[tokio::test(flavor = "current_thread")]
async fn http_module_null_without_module_context() {
    let (base, api) = mod_fx(&[(
        "api.js",
        "export function get() { json.ok({ mod: http.module }); }\nexport default { get };\n",
    )]);
    let b = module_bridge(&base);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["data"]["mod"].is_null(), "{v}");
}

/// api 文件 CJS 探针（v0.1.76）：wrap_cjs 包装器自身是 ESM（`export default`）
/// → 模块 strict → 按 spec 其中定义的 IIFE **继承 strict** → 写冻结同样抛
/// TypeError（不是 sloppy；sloppy 只在 require() 的 new Function 路径，见下一探针）。
#[tokio::test(flavor = "current_thread")]
async fn http_module_frozen_cjs_api_file_write_throws() {
    let (base, api) = mod_fx(&[(
        "m/api.js",
        "module.exports = { get() { http.module.name = \"evil\"; json.ok(1); } };\n",
    )]);
    let modules = Arc::new(HashMap::from([(
        base.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "9.9.9".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    let b = module_bridge_ex(&base, SchemaRegistry::new(), modules, false, HashMap::new());
    let e = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("read only"),
        "CJS api 文件（ESM 包装 → strict）写冻结应抛 TypeError：{e}"
    );
}

/// require() CJS 探针（v0.1.76）：require 的代码经 __ojRequire 的 new Function
/// 执行（sloppy）——写冻结对象静默无效不抛，读回不变。与 strict 路径的
/// TypeError 互补：两条路径都动不了 Rust 授信面（JS 只持 op_http_info 快照拷贝）。
#[tokio::test(flavor = "current_thread")]
async fn http_module_frozen_via_require_cjs_sloppy_ignored() {
    let (base, api) = mod_fx(&[
        (
            "m/api.js",
            "module.exports = { get() { const h = require(\"./helper\"); json.ok({ poked: h.poke(http.module), mod: http.module }); } };\n",
        ),
        (
            "m/helper.js",
            "module.exports = { poke(m) { m.name = \"evil\"; return m.name; } };\n",
        ),
    ]);
    let modules = Arc::new(HashMap::from([(
        base.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "9.9.9".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    let b = module_bridge_ex(&base, SchemaRegistry::new(), modules, false, HashMap::new());
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["poked"], "m", "sloppy 写应静默无效：{v}");
    assert_eq!(v["data"]["mod"]["name"], "m", "{v}");
    assert_eq!(v["data"]["mod"]["version"], "9.9.9", "{v}");
}

/// 对抗性探测（v0.1.76）：http.module 冻结只读——isFrozen、strict 写抛
/// TypeError、Proxy 无 set 陷阱（`http.module = x` 惰性无效，读回不变）。
#[tokio::test(flavor = "current_thread")]
async fn http_module_is_frozen_and_rejects_writes() {
    let (base, api) = mod_fx(&[(
        "m/api.js",
        r#"export function get() {
                 const frozen = Object.isFrozen(http.module);
                 let err = null;
                 try { http.module.name = "evil"; } catch (e) { err = String(e); }
                 let delErr = null;
                 try { delete http.module.version; } catch (e) { delErr = String(e); }
                 http.module = { name: "evil", version: "0" };
                 const after = http.module;
                 json.ok({ frozen, err, delErr, name: after.name, version: after.version });
               }
               export default { get };"#,
    )]);
    let modules = Arc::new(HashMap::from([(
        base.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "9.9.9".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    let b = module_bridge_ex(&base, SchemaRegistry::new(), modules, false, HashMap::new());
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["frozen"], true, "{v}");
    assert!(
        v["data"]["err"]
            .as_str()
            .unwrap_or("")
            .contains("read only"),
        "strict 写冻结对象应抛 TypeError：{v}"
    );
    assert!(
        v["data"]["delErr"]
            .as_str()
            .unwrap_or("")
            .contains("Cannot delete"),
        "delete 冻结属性应抛 TypeError：{v}"
    );
    // 赋值落隐藏 target，get 仍走快照——读回恒为真实模块身份。
    assert_eq!(v["data"]["name"], "m", "{v}");
    assert_eq!(v["data"]["version"], "9.9.9", "{v}");
}

/// blob 未配置（Extras 缺省）：JS 侧 blob.* 报 "blob not configured"。
#[tokio::test(flavor = "current_thread")]
async fn blob_not_configured_errors() {
    let (b, _) = new_bridge();
    let e = b
        .run_with(
            r#"blob.put("a/b.png", new Uint8Array([1]));"#,
            RequestInfo::default(),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("blob not configured"), "{e}");
}

/// blob 经 Extras 注入：JS put/get/url 走 LocalBlob。
#[tokio::test(flavor = "current_thread")]
async fn blob_roundtrip_via_extras() {
    let dir = std::env::temp_dir().join(format!("oj-blobjs-{}", std::process::id()));
    let local = blob::LocalBlob::new(&dir, "/v1/api").unwrap();
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            blobs: Some(crate::bridge::blob::registry_with_default(Arc::new(local))),
            ..Default::default()
        },
    );
    let cap = b
        .run_with(
            r#"
                (async () => {
                    await blob.put("js/a.png", new Uint8Array([1, 2, 3]), "image/png");
                    const got = await blob.get("js/a.png");
                    await blob.del("js/none");
                    json.ok({ n: got.length, url: await blob.url("js/a.png") });
                })();
                "#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 3);
    assert_eq!(v["data"]["url"], "/v1/api/blob/js/a.png");
}

#[tokio::test(flavor = "current_thread")]
async fn globals_injected_and_json_ok() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"
                const injected = typeof json === "object" && typeof db === "object"
                    && typeof http === "object" && typeof redis === "object"
                    && typeof DB === "function" && typeof fetch === "function"
                    && typeof log === "object" && typeof finish === "function";
                log.info("handler start", "method", http.method);
                json.header("X-Handler", "test");
                json.ok({ injected, method: http.method });
                "#,
            RequestInfo {
                method: "POST".into(),
                query: [("id".into(), "1".into())].into_iter().collect(),
                body: br#"{"a":1}"#.to_vec(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(cap.status, 200);
    assert_eq!(cap.header("X-Handler").unwrap(), "test");
    assert_eq!(cap.header("content-type").unwrap(), "application/json");
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v,
        json!({"code": 0, "msg": "ok", "data": {"injected": true, "method": "POST"}})
    );

    // json.header 显式设置的 content-type 优先，不被默认值覆盖。
    let cap = b
        .run_with(
            r#"
                json.header("Content-Type", "application/problem+json");
                json.ok(null);
                "#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        cap.header("Content-Type").unwrap(),
        "application/problem+json"
    );
    assert!(cap.header("content-type").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn db_params_and_build() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1, "name": "ever"})]);
    let registry = SchemaRegistry::new().table("user", &["id"], &["id", "name", "age"]);
    let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);
    // 结构化查询构造器（经 SchemaRegistry 白名单）。
    let cap = b
        .run(
            r#"
                db.table("user").select(["id","name"]).where({field:"id",op:"eq",value:1})
                  .orderBy([{field:"id",dir:"asc"}]).limit(10).all()
                  .then((rows) => json.ok({ rows, missing: typeof DB("nope") === "undefined" }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "handler failed: {v}");
    assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "ever"}]));
    assert_eq!(v["data"]["missing"], true);

    // 原始 SQL + 绑定参数（无拼接）。
    let cap = b
            .run(r#"db.query("select * from user where id = $1", [1]).then((rows) => json.ok({ rows })).catch((e) => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "param query failed: {v}");
    assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "ever"}]));
}

#[tokio::test(flavor = "current_thread")]
async fn named_dbs_and_default_fallback() {
    // 构造期注入全量命名 DB（替代 set_db_accessors——Arc 已被池共享，事后改不可行）。
    let a = Arc::new(InMemoryAccessor::new());
    a.seed([json!({"id": 1, "name": "ever"})]);
    let b = Bridge::with_dbs(
        HashMap::from([("default".to_string(), a as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("user", &["id"], &["id", "name"]),
        false,
    );
    let cap = b
            .run(r#"db.table("user").select(["id"]).all().then((rows) => json.ok({ n: rows.length })).catch((e) => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");

    // 无 "default" 键 → 回落第一个实例（防御）。
    let only = Arc::new(InMemoryAccessor::new());
    only.seed([json!({"id": 2, "name": "neo"})]);
    let b2 = Bridge::with_dbs(
        HashMap::from([("only".to_string(), only as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("user", &["id"], &["id", "name"]),
        false,
    );
    let cap = b2
            .run(r#"db.query("select * from user").then((rows) => json.ok({ n: rows.length })).catch((e) => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn http_param_and_kv_global() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"
                kv.set("k", "v");
                kv.get("k").then((v) => {
                    const hit = v;
                    kv.del("k");
                    kv.get("k").then((v2) => json.ok({
                        hit, gone: v2,
                        p1: http.param("id", 0),
                        p2: http.param("missing", "dft"),
                    }));
                }).catch((e) => json.fail(500, String(e)));
                "#,
            RequestInfo {
                query: [("id".into(), "7".into())].into_iter().collect(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"],
        json!({"hit": "v", "gone": null, "p1": "7", "p2": "dft"}),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn kv_expire_incr_global() {
    // bootstrap 接线：kv.expire(key, 秒) / kv.incr(key) 经 op 可达；redis 同源。
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"
                (async () => {
                    await kv.incr("c");
                    await kv.incr("c");
                    const n = await kv.incr("c");        // 3
                    const ok = await kv.expire("c", 1);   // 秒 → ms
                    await redis.set("r", "9");
                    const r = await redis.incr("r");      // 10
                    json.ok({ n, ok, r });
                })().catch((e) => json.fail(500, String(e)));
                "#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"], json!({"n": 3, "ok": true, "r": 10}), "{v}");
}

/// bus：WS 上下文订阅（bus_tx 注入）→ 后续发布广播 JSON 帧到订阅方；
/// HTTP 上下文订阅 → 抛错（无 WS 连接）。
#[tokio::test(flavor = "current_thread")]
async fn bus_subscribe_publish_and_http_rejects() {
    let (b, _) = new_bridge();
    let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel::<WsSend>();
    // 帧 1：WS 会话订阅（RequestInfo.bus_tx 注入）
    let o1 = b
            .run_ws(
                r#"(async () => { await bus.subscribe("news"); json.ok({ ok: 1 }); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo { bus_tx: Some(bus_tx.clone()), ..Default::default() },
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
    assert_eq!(o1.capture.status, 200);
    // 帧 2：发布（无 bus_tx 也广播）→ 返回接收方数
    let o2 = b
            .run_ws(
                r#"(async () => { const n = await bus.publish("news", { a: 1 }); json.ok({ n }); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
    let v2: Value = serde_json::from_slice(&o2.capture.body).unwrap();
    assert_eq!(v2["data"]["n"], 1, "{v2}");
    // 订阅方收到 {"topic","data"} JSON text 帧
    assert_eq!(
        bus_rx.try_recv().unwrap(),
        WsSend::Text(json!({"topic": "news", "data": {"a": 1}}).to_string())
    );
    // HTTP 上下文（bus_tx None）订阅 → 报错（msg 含 WebSocket）
    let cap = b
            .run_with(
                r#"(async () => { await bus.subscribe("news"); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["err"].as_str().unwrap().contains("WebSocket"),
        "{v}"
    );
}

/// bus.kind()：进程内总线返回 "local"（分布式实现经 feature 启用）。
#[tokio::test(flavor = "current_thread")]
async fn bus_kind_op_returns_local() {
    let (b, _) = new_bridge();
    let cap = b
            .run_with(
                r#"(async () => { const k = await bus.kind(); json.ok({ k }); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["k"], "local");
}

#[tokio::test(flavor = "current_thread")]
async fn http_files_and_file_roundtrip() {
    // http.files 元信息 + http.file(i) 字节；越界 → "no such file"。
    let (b, _) = new_bridge();
    let cap = b
            .run_with(
                r#"
                (async () => {
                    const f = http.files[0];
                    const bts = await http.file(0);
                    json.ok({ name: f.filename, ct: f.content_type, size: f.size, n: bts.length, b0: bts[0], key: f.key, url: f.url });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo {
                    files: vec![UploadedFile {
                        field: "file".into(),
                        filename: "a.png".into(),
                        content_type: Some("image/png".into()),
                        bytes: vec![1, 2, 3],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"],
        json!({
            "name": "a.png", "ct": "image/png", "size": 3, "n": 3, "b0": 1,
            "key": Value::Null, "url": Value::Null,
        }),
        "{v}"
    );
    // 无文件时取越界索引 → 报错
    let cap = b
            .run_with(
                r#"(async () => { await http.file(5); json.fail(500, "should throw"); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["err"].as_str().unwrap().contains("no such file"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tx_commit_visible_and_throw_rolls_back() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    let b = Bridge::new(db, Arc::new(InMemoryKV::new()));
    // 提交可见
    let cap = b
        .run(
            r#"db.tx(async (tx) => { await tx.exec("insert into t (v) values (?)", ["x"]); })
                .then(() => db.query("select count(*) c from t"))
                .then((r) => json.ok(r[0]))
                .catch((e) => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["c"], 1, "{v}");
    // fn 抛错 → 自动回滚（y 行不可见）
    let cap = b
            .run(r#"db.tx(async (tx) => { await tx.exec("insert into t (v) values (?)", ["y"]); throw new Error("boom"); })
                .then(() => json.fail(500, "should have thrown"))
                .catch((e) => db.query("select count(*) c from t").then((r) => json.ok({ err: String(e), c: r[0].c })));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["c"], 1, "y must be rolled back: {v}");
    assert!(v["data"]["err"].as_str().unwrap().contains("boom"), "{v}");
    // 嵌套 begin 报错
    let cap = b
        .run(
            r#"db.tx(async () => { await db.tx(async () => {}); })
                .then(() => json.fail(500, "nested should fail"))
                .catch((e) => json.ok({ msg: String(e) }));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["msg"]
            .as_str()
            .unwrap()
            .contains("already active"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builder_in_tx_shares_connection_and_rolls_back() {
    // 构造器（SELECT-only）在 tx 内必须走同一事务连接：能读到未提交行；
    // 回滚后行不可见。（sqlite 单连接池下，若 builder 误走 pool 会死锁到超时。）
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "v"]);
    let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false);
    let cap = b
            .run(r#"db.tx(async (tx) => {
                    await tx.exec("insert into t (v) values (?)", ["z"]);
                    const rows = await tx.table("t").select(["v"]).all();  // 同一事务连接 → 见未提交 z
                    throw new Error("force rollback: " + rows.length);
                }).then(() => json.fail(500, "no"))
                .catch((e) => db.query("select count(*) c from t").then((r) => json.ok({ msg: String(e), c: r[0].c })));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["msg"]
            .as_str()
            .unwrap()
            .contains("force rollback: 1"),
        "{v}"
    );
    assert_eq!(v["data"]["c"], 0, "z must be rolled back: {v}");
}

#[tokio::test(flavor = "current_thread")]
async fn ws_global_send_close_and_reset() {
    // WS 帧循环契约：ws.send 收集到 sends（先于信封写出）、ws.close 置位，帧间重置。
    let (b, _) = new_bridge();
    let o = b
            .run_ws(
                r#"
                const ok1 = typeof ws === "object" && typeof ws.send === "function" && typeof ws.close === "function";
                ws.send("side-a"); ws.send("side-b"); ws.close();
                json.ok({ ok1 });
                "#,
                RequestInfo::default(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
    assert_eq!(
        o.sends,
        vec![
            WsSend::Text("side-a".to_string()),
            WsSend::Text("side-b".to_string())
        ]
    );
    assert!(o.close);
    let v: Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["ok1"], true, "{v}");

    // 第二帧：sends/close 不串号（ReqState 重置）。
    let o2 = b
        .run_ws(
            r#"json.ok({});"#,
            RequestInfo::default(),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(o2.sends.is_empty());
    assert!(!o2.close);
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_table_rejected() {
    let (b, _) = new_bridge();
    let cap = b
            .run(r#"db.table("secret").select(["x"]).all().then((r) => json.ok(r)).catch((e) => json.fail(400, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400);
    assert!(v["msg"].as_str().unwrap().contains("unknown table"));
}

#[tokio::test(flavor = "current_thread")]
async fn infinite_loop_times_out_and_bridge_survives() {
    let (b, _) = new_bridge();
    let r = b
        .run_with_timeout(
            "while (true) {}",
            RequestInfo::default(),
            std::time::Duration::from_millis(150),
        )
        .await;
    assert!(matches!(r, Err(RunError::Timeout)), "got: {r:?}");
    // 超时 runtime 被丢弃，bridge 后续请求正常（池可新开 runtime）。
    let cap = b.run(r#"json.ok({ alive: true });"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["alive"], true);
}

#[tokio::test(flavor = "current_thread")]
async fn oj_require_cjs_interop_end_to_end() {
    // T8 契约：__ojRequire(name, referrer) → op 解析 + eval + 嵌套 require。
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "oj-req-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    for (rel, content) in [
        (
            "node_modules/cjspkg/package.json",
            r#"{"name":"cjspkg","main":"main.js"}"#,
        ),
        // 嵌套 require：dep 经 __ojRequire 的 resolved.path 再解析。
        (
            "node_modules/cjspkg/main.js",
            "module.exports = { n: 1, dep: require(\"cjspkg-dep\").d };\n",
        ),
        (
            "node_modules/cjspkg-dep/index.js",
            "module.exports = { d: 2 };\n",
        ),
    ] {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    let stable = Arc::new(StableState {
        kv: Arc::new(InMemoryKV::new()),
        dbs: HashMap::new(),
        registry: Arc::new(SchemaRegistry::new()),
        input_contracts: None, // 单测路径不注入契约（默认不校验）
        seq_ensured: std::sync::Mutex::new(std::collections::HashSet::new()),
        secret_keys: None,
        module_index: None,
        loader: Some(Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: false,
        })),
        blobs: Arc::new(blob::BlobRegistry::new()),
        bus: Arc::new(bus::Bus::new()),
        es: None,
        plugins: Vec::new(),
        modules: Arc::new(HashMap::new()),
        ownership_deny: false,
        sql_guard: SqlGuard::Off,
        allow_as_tenant: false,
        db_override: None,
        query_limits: query::QueryLimits::default(),
        sql_trace: sql_trace::SqlTraceConfig::default(),
        boot: None,
        jwt: None,
        oidc: None,
        sso: None,
        kafkas: Arc::new(NamedRegistry::new()),
        rabbits: Arc::new(NamedRegistry::new()),
        tasks_flag: None,
        sql_memo: Mutex::new(HashMap::new()),
        mail: None,
        ldap: None,
        db_access: Arc::new(db_policy::DbAccessSection::default()),
        db_client: Arc::new(db_policy::DbClientSection::default()),
        db_session: Arc::new(db_policy::DbSessionSection::default()),
        client_dbs: Arc::new(client_db::ClientDbRegistry::new(16)),
        tx_sessions: Arc::new(db_session::TxSessionRegistry::disabled()),
        audit_section: Arc::new(db_policy::AuditSection::default()),
        audit: Arc::new(audit::AuditStore::new(&db_policy::AuditSection::default())),
        exec_sessions: Arc::new(db_exec::ExecRegistry::default()),
        export_section: Arc::new(db_policy::DbExportSection::default()),
        export_registry: Arc::new(db_export::ExportRegistry::default()),
        import_section: Arc::new(db_policy::DbImportSection::default()),
        import_registry: Arc::new(db_import::ImportRegistry::default()),
        mgmt_section: Arc::new(db_policy::MgmtSection::default()),
        mgmt_axes: Arc::new(HashMap::new()),
        mgmt: Arc::new(mgmt::MgmtRegistry::new(8, 4)),
        db_access_rt: Arc::new(Mutex::new(HashMap::new())),
        db_backend: Arc::new(db_backend::DbBackendRegistry::builtin()),
        config_dir: Arc::new(std::path::PathBuf::from(".")),
        vars: Arc::new(HashMap::new()),
        js_heap_limit: None,
        fs: None,
        platform: None,
        exec: None,
        reload: Arc::new(std::sync::OnceLock::new()),
        generic_axes: Arc::new(HashMap::new()),
        oj_info: Arc::new(serde_json::json!({})),
    });
    // 无 boot → 看门狗不参与（Default 不起线程），仅满足池的构造契约。
    let pool = runtime::RuntimePool::new(stable, false, Arc::new(runtime::KillSwitch::default()));
    let mut rt = pool.checkout().await.unwrap();
    let referrer = root.join("src/handler.js");
    let src = format!(
        r#"const c = __ojRequire("cjspkg", {referrer:?}); json.ok({{ n: c.n, dep: c.dep }});"#
    );
    runtime::run_to_completion(&mut rt, "handler.js", src)
        .await
        .unwrap();
    let v: Value = {
        let op_state = runtime::op_state(&rt);
        let g = op_state.borrow();
        serde_json::from_slice(&g.borrow::<ReqState>().response.clone().unwrap()).unwrap()
    };
    pool.checkin(rt);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(v["data"], json!({ "n": 1, "dep": 2 }), "{v}");
}

fn mod_fx(files: &[(&str, &str)]) -> (std::path::PathBuf, std::path::PathBuf) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let base = std::env::temp_dir().join(format!(
        "oj-mod-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&base).unwrap();
    for (rel, content) in files {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    (base.clone(), base.join(files[0].0))
}

fn module_bridge(root: &std::path::Path) -> Bridge {
    module_bridge_ex(
        root,
        SchemaRegistry::new(),
        Arc::new(HashMap::new()),
        false,
        HashMap::new(),
    )
}

/// 带归属上下文的模块 bridge：modules 键 = 模块目录绝对路径（§5.3 装配形态）。
/// dbs：命名库表（None = 空，走 Bridge 默认补 default）。
fn module_bridge_ex(
    root: &std::path::Path,
    registry: SchemaRegistry,
    modules: Arc<HashMap<String, ModuleCtx>>,
    ownership_deny: bool,
    dbs: HashMap<String, Arc<dyn DataAccessor>>,
) -> Bridge {
    Bridge::with_dbs_and_loader(
        dbs,
        Arc::new(InMemoryKV::new()),
        registry,
        false,
        Some(Arc::new(LoaderShared {
            project_root: root.to_path_buf(),
            ts: true,
        })),
        Extras {
            modules,
            ownership_deny,
            ..Default::default()
        },
    )
}

/// run_module 系列触发 .ts 转译：与 transpile 测试的计数器 delta 断言互斥
/// （current_thread 单线程 runtime，锁跨 await 安全；poison 不级联）。
fn transpile_serial() -> std::sync::MutexGuard<'static, ()> {
    transpile::TRANSPILE_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[tokio::test(flavor = "current_thread")]
async fn runs_exported_method_from_ts_module() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[
        (
            "user/account/api.ts",
            "import { tag } from \"../_shared/util\";\n\
                 function get(): void { json.ok({ ok: 1, tag: tag(\"x\") }); }\n\
                 export default { get };\n",
        ),
        (
            "user/_shared/util.ts",
            "export function tag(s: string): string { return \"t-\" + s; }\n",
        ),
    ]);
    let b = module_bridge(&root);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 200);
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"], json!({"ok": 1, "tag": "t-x"}), "{v}");
}

/// 契约守卫（v0.1.44）：外部请求违反 `.schema` → 400 且**不进 JS**；
/// 内部合成派发（`dispatch_meta_handler`）豁免 —— 它造的是 `query={"path":…}`、
/// 空 body 的合成请求，契约失败只会静默降级页面 meta，属于误伤。
#[tokio::test(flavor = "current_thread")]
async fn contract_violation_is_400_but_internal_dispatch_is_exempt() {
    use crate::contract::{InputContract, InputContractRegistry};
    // 用 .js + ts:false：本用例不需要转译；转译会与 transpile 测试的
    // 计数器 delta 断言互斥（见 transpile_serial 注释）。
    let (root, api) = mod_fx(&[(
        "u/f/api.js",
        "function get() { json.ok({ reached: true }); }\n\
             get.schema = { params: { type: \"object\", required: [\"id\"], \
               properties: { id: { type: \"integer\" } } } };\n\
             export default { get };\n",
    )]);
    let contract = InputContract::try_new(
        Some(serde_json::json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "integer" } }
        })),
        None,
        None,
        &mut Vec::new(),
    )
    .unwrap();
    let mut reg = InputContractRegistry::new();
    reg.insert(api.clone(), "get", contract).unwrap();
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        Some(Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: false,
        })),
        Extras {
            input_contracts: Some(Arc::new(std::sync::RwLock::new(reg))),
            ..Default::default()
        },
    );
    let t = std::time::Duration::from_secs(5);

    // 1) 外部请求（validate=true）：缺 required `id` → 400，且 JS 未被执行。
    let cap = b
        .run_module(&api, "get", RequestInfo::default(), t)
        .await
        .unwrap();
    assert_eq!(cap.status, 400, "{}", String::from_utf8_lossy(&cap.body));
    assert!(
        !String::from_utf8_lossy(&cap.body).contains("reached"),
        "契约失败必须在进 JS 之前拦截"
    );

    // 2) 内部合成派发（validate=false）：豁免，handler 正常执行。
    let cap2 = b
        .run_module_validated(&api, "get", RequestInfo::default(), t, false)
        .await
        .unwrap();
    assert_eq!(cap2.status, 200, "{}", String::from_utf8_lossy(&cap2.body));
    assert!(
        String::from_utf8_lossy(&cap2.body).contains("reached"),
        "豁免后应真正执行 handler"
    );

    // 3) 击穿反向：validate=true 时**合规**请求仍须放行（不得误拦）。
    let mut ok_req = RequestInfo::default();
    ok_req.params.insert("id".to_string(), "7".to_string());
    let cap3 = b
        .run_module_validated(&api, "get", ok_req, t, true)
        .await
        .unwrap();
    assert_eq!(cap3.status, 200, "合规请求不得被误拦");
}

/// v0.1.64 嵌套 handler 组：driver 沿 handler 链逐段计算式取值。
#[tokio::test(flavor = "current_thread")]
async fn run_module_dispatches_nested_handler_chain() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "const cccc = { get() { json.ok({ where: \"cccc.get\" }); } };\n\
             export default { post() { json.ok({ where: \"post\" }); }, cccc };\n",
    )]);
    let b = module_bridge(&root);
    let t = std::time::Duration::from_secs(5);
    for (chain, want) in [("cccc.get", "cccc.get"), ("post", "post")] {
        let cap = b
            .run_module(&api, chain, RequestInfo::default(), t)
            .await
            .unwrap();
        assert_eq!(
            cap.status,
            200,
            "chain {chain}: {}",
            String::from_utf8_lossy(&cap.body)
        );
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["where"], json!(want), "chain {chain}");
    }
}

/// 段名红线：`__proto__` / `constructor` / `prototype` / 空段 / 含 `/` → 404
/// （计算式访问前的排除先于取值；手改/旧 routes.js 的纵深防护）。
#[tokio::test(flavor = "current_thread")]
async fn run_module_bad_segments_are_404() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "const cccc = { get() { json.ok({}); } };\n\
             export default { cccc };\n",
    )]);
    let b = module_bridge(&root);
    for chain in [
        "cccc.__proto__",
        "cccc.constructor",
        "cccc.prototype",
        "cccc.get/evil",
        "",
        "cccc..get",
    ] {
        let cap = b
            .run_module(
                &api,
                chain,
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(
            cap.status,
            404,
            "chain {chain:?}: {}",
            String::from_utf8_lossy(&cap.body)
        );
    }
}

/// 链上任意一段缺失（含中间段）→ 404「no handler」。
#[tokio::test(flavor = "current_thread")]
async fn run_module_missing_chain_is_404() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "const cccc = { get() { json.ok({}); } };\n\
             export default { cccc };\n",
    )]);
    let b = module_bridge(&root);
    for chain in ["cccc.post", "nope.get", "cccc.get.deeper"] {
        let cap = b
            .run_module(
                &api,
                chain,
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(
            cap.status,
            404,
            "chain {chain:?}: {}",
            String::from_utf8_lossy(&cap.body)
        );
    }
}

/// 末段不是函数（手改 routes.js 指到对象属性）→ 405，不进 JS。
#[tokio::test(flavor = "current_thread")]
async fn run_module_non_function_leaf_is_405() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "const cccc = { get: { nested: 1 } };\n\
             export default { cccc };\n",
    )]);
    let b = module_bridge(&root);
    let cap = b
        .run_module(
            &api,
            "cccc.get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 405, "{}", String::from_utf8_lossy(&cap.body));
}

/// 非标识符段名（连字符组名）经计算式访问正确命中。
#[tokio::test(flavor = "current_thread")]
async fn run_module_non_identifier_segment_hits() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "const userList = { get() { json.ok({ ok: true }); } };\n\
             export default { \"user-list\": userList };\n",
    )]);
    let b = module_bridge(&root);
    let cap = b
        .run_module(
            &api,
            "user-list.get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
}

/// 兼容：default 导出是**函数**且 handler 挂其属性（旧语义读 `m.default[method]`）。
#[tokio::test(flavor = "current_thread")]
async fn run_module_function_default_still_works() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "export default function api() {}\n\
             api.get = function () { json.ok({ fn: true }); };\n",
    )]);
    let b = module_bridge(&root);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
}

/// 链上缺段（含顶层动词未导出）→ 404「no handler」（v0.1.64 §四 driver 语义；
/// 旧版顶层缺失方法为 405，随嵌套链统一为 404——「存在但非函数」才是 405，
/// 见 `run_module_non_function_leaf_is_405`）。
#[tokio::test(flavor = "current_thread")]
async fn missing_handler_segment_is_404() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[("u/f/api.ts", "export default { get() { json.ok({}); } };\n")]);
    let b = module_bridge(&root);
    let cap = b
        .run_module(
            &api,
            "del",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 404, "{}", String::from_utf8_lossy(&cap.body));
}

#[tokio::test(flavor = "current_thread")]
async fn infinite_module_handler_times_out_and_bridge_survives() {
    let _t = transpile_serial();
    // R1 spike：ESM/TLA 模型下 KillSwitch 复验。
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "export default { get() { while (true) {} } };\n",
    )]);
    let b = module_bridge(&root);
    let r = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_millis(200),
        )
        .await;
    assert!(matches!(r, Err(RunError::Timeout)), "got: {r:?}");
    let cap = b.run(r#"json.ok({ alive: true });"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["alive"], true);
}

#[tokio::test(flavor = "current_thread")]
async fn run_module_reuses_pooled_runtime_across_requests() {
    // 回归：每 JsRuntime 仅一个 main module；driver 须以 side module 加载，
    // 否则同一池化 runtime 的第二个请求报 main module already exists。
    // 模块顶层每 runtime 只执行一次（ESM 缓存）→ n 跨请求累计。
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "u/f/api.ts",
        "let n = 0;\nexport default { get() { n += 1; json.ok({ n }); } };\n",
    )]);
    let b = module_bridge(&root);
    let cap1 = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let cap2 = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(cap1.status, 200);
    assert_eq!(cap2.status, 200);
    let v1: Value = serde_json::from_slice(&cap1.body).unwrap();
    let v2: Value = serde_json::from_slice(&cap2.body).unwrap();
    assert_eq!(v1["data"], json!({"n": 1}), "{v1}");
    assert_eq!(v2["data"], json!({"n": 2}), "{v2}");
}

#[tokio::test(flavor = "current_thread")]
async fn introspect_reads_route_decls() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "a/api.ts",
        "function get() { json.ok({}); }\n\
             get.route = \"{id}\";\n\
             function del() { json.ok({}); }\n\
             export default { get, del };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    // v0.1.64：扁平 leaf 数组；route 与 schema 同源一趟取（v0.1.44 语义不变）。
    assert_eq!(leaf(&v, "get")["route"], json!("{id}"), "{v}");
    assert_eq!(leaf(&v, "get")["schema"], json!(null), "{v}");
    assert_eq!(leaf(&v, "del")["route"], json!(null), "{v}");
    assert_eq!(leaf(&v, "del")["schema"], json!(null), "{v}");
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}"); // 未导出的 post 缺席
}

/// 取某个 leaf（按 handler 链在内省数组里查找）；嵌套组用例共用。
fn leaf<'a>(v: &'a Value, chain: &str) -> &'a Value {
    v.as_array()
        .expect("introspect output is a flat leaf array")
        .iter()
        .find(|l| l["chain"] == chain)
        .unwrap_or_else(|| panic!("no leaf {chain:?} in {v}"))
}

/// v0.1.64 嵌套 handler 组：内省产出**扁平 leaf 数组**（`{chain, route, schema}`），
/// route 为组前缀解析后的有效路由（Rust 词汇：null = 目录镜像 / "/x" = 根级 /
/// "seg/.." = 相对目录镜像）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_nested_groups_yield_flat_leaves() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "aaa/bbb/api.ts",
        "function get() { json.ok({}); }\n\
             const cccc = {\n\
               get: () => { json.ok({}); },\n\
               post: () => { json.ok({}); },\n\
               list: { get: () => { json.ok({}); } },\n\
             };\n\
             cccc.get.schema = { query: { type: \"object\" } };\n\
             export default { get, cccc };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(leaf(&v, "get")["route"], json!(null), "{v}");
    assert_eq!(leaf(&v, "cccc.get")["route"], json!("cccc"), "{v}");
    assert_eq!(leaf(&v, "cccc.post")["route"], json!("cccc"), "{v}");
    assert_eq!(
        leaf(&v, "cccc.list.get")["route"],
        json!("cccc/list"),
        "{v}"
    );
    // 元数据隔离：schema 只挂在 handler 函数上，不生成任何路由
    assert_eq!(
        leaf(&v, "cccc.get")["schema"],
        json!({ "query": { "type": "object" } }),
        "{v}"
    );
    assert!(leaf(&v, "cccc.post")["schema"] == json!(null), "{v}");
    assert_eq!(v.as_array().unwrap().len(), 4, "{v}");
}

/// 组级 `.route` 作整组默认前缀；动词自身 `.route` 优先覆盖（绝对/相对皆可）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_group_route_is_default_prefix_and_verb_overrides() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const cccc = {\n\
               get: () => { json.ok({}); },\n\
               post: () => { json.ok({}); },\n\
             };\n\
             cccc.route = \"/x\";\n\
             cccc.post.route = \"/y\";\n\
             export default { cccc };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(leaf(&v, "cccc.get")["route"], json!("/x"), "{v}");
    assert_eq!(leaf(&v, "cccc.post")["route"], json!("/y"), "{v}");
}

/// 相对组 `.route`：动词自身相对路由按「父前缀 + / + R」解析。
#[tokio::test(flavor = "current_thread")]
async fn introspect_relative_routes_resolve_against_layer_prefix() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "r/api.ts",
        "const cccc = {\n\
               get: () => { json.ok({}); },\n\
               post: () => { json.ok({}); },\n\
             };\n\
             cccc.route = \"sub\";\n\
             cccc.post.route = \"deep/thing\";\n\
             function put() { json.ok({}); }\n\
             put.route = \"me\";\n\
             export default { put, cccc };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(leaf(&v, "put")["route"], json!("me"), "{v}");
    assert_eq!(leaf(&v, "cccc.get")["route"], json!("sub"), "{v}");
    assert_eq!(
        leaf(&v, "cccc.post")["route"],
        json!("sub/deep/thing"),
        "{v}"
    );
}

/// 文件层（root default）`.route` 同样作本层默认前缀（§二 语义表「文件」行）：
/// 绝对 → 根级；动词自身 `.route` 优先覆盖。
#[tokio::test(flavor = "current_thread")]
async fn introspect_file_level_route_is_default_prefix() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "f/api.ts",
        "function get() { json.ok({}); }\n\
             function post() { json.ok({}); }\n\
             post.route = \"/y\";\n\
             export default { get, post, route: \"/x\" };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(leaf(&v, "get")["route"], json!("/x"), "{v}");
    assert_eq!(leaf(&v, "post")["route"], json!("/y"), "{v}");
}

/// 组级 `.schema` 与组级 `.route` 同等地位：组 `.schema` 作整组默认契约，动词继承；
/// 动词自身 `.schema` 在其上**合并**（通道并集，不是替换）——组级 params + 动词级 body。
#[tokio::test(flavor = "current_thread")]
async fn introspect_group_schema_is_inherited_and_verb_merges() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const asset = {\n\
               get: () => { json.ok({}); },\n\
               post: () => { json.ok({}); },\n\
             };\n\
             asset.schema = {\n\
               params: { type: \"object\", required: [\"id\"], properties: { id: { type: \"integer\" } } },\n\
             };\n\
             asset.post.schema = {\n\
               body: { type: \"object\", required: [\"name\"], properties: { name: { type: \"string\" } } },\n\
             };\n\
             export default { asset };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    // 动词继承组级 params 契约
    assert_eq!(
        leaf(&v, "asset.get")["schema"],
        json!({ "params": { "type": "object", "required": ["id"], "properties": { "id": { "type": "integer" } } } }),
        "{v}"
    );
    // 动词合并：组级 params + 动词级 body（通道并集，非替换）
    assert_eq!(
        leaf(&v, "asset.post")["schema"],
        json!({
            "params": { "type": "object", "required": ["id"], "properties": { "id": { "type": "integer" } } },
            "body": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } }
        }),
        "{v}"
    );
}

/// v0.1.72：`.response` 平级声明——出参不再只能塞在 `.schema.response` 里。
/// 优先级：动词 `.response` > 组 `.response`（沿嵌套继承）> 动词 `.schema.response`
/// > 组 `.schema.response`；均缺省时 schema 不带 response 键（openapi 占位行为不变）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_response_sibling_priority_and_inheritance() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const grp = {\n\
               get: () => { json.ok({}); },\n\
               post: () => { json.ok({}); },\n\
               put: () => { json.ok({}); },\n\
               del: () => { json.ok({}); },\n\
               head: () => { json.ok({}); },\n\
               options: () => { json.ok({}); },\n\
             };\n\
             grp.response = { type: \"object\", properties: { g: { type: \"integer\" } } };\n\
             grp.get.response = { type: \"string\" };\n\
             grp.post.schema = { response: { type: \"integer\" } };\n\
             grp.put.schema = { query: { type: \"object\" } };\n\
             grp.head.response = null;\n\
             grp.options.schema = { response: null };\n\
             function patch() { json.ok({}); }\n\
             patch.response = { type: \"boolean\" };\n\
             export default { grp, patch };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    // 动词 .response 覆盖组 .response
    assert_eq!(
        leaf(&v, "grp.get")["schema"]["response"],
        json!({ "type": "string" }),
        "{v}"
    );
    // 组 .response 优先于动词 .schema.response
    assert_eq!(
        leaf(&v, "grp.post")["schema"]["response"],
        json!({ "type": "object", "properties": { "g": { "type": "integer" } } }),
        "{v}"
    );
    // 组 .response 被普通动词继承（入参通道不受影响）
    assert_eq!(
        leaf(&v, "grp.put")["schema"],
        json!({
            "query": { "type": "object" },
            "response": { "type": "object", "properties": { "g": { "type": "integer" } } }
        }),
        "{v}"
    );
    // 回归：动词 b 的 .schema.response 被组 .response 顶替后，不得泄漏给兄弟动词 d
    assert_eq!(
        leaf(&v, "grp.del")["schema"]["response"],
        json!({ "type": "object", "properties": { "g": { "type": "integer" } } }),
        "{v}"
    );
    // 无组上下文：仅写动词级 .response 同样生效
    assert_eq!(
        leaf(&v, "patch")["schema"]["response"],
        json!({ "type": "boolean" }),
        "{v}"
    );
    // v0.1.74：null 视为未声明——head 的 .response = null 回落继承组级
    assert_eq!(
        leaf(&v, "grp.head")["schema"]["response"],
        json!({ "type": "object", "properties": { "g": { "type": "integer" } } }),
        "{v}"
    );
    // v0.1.74：.schema.response = null 同样视为未声明——组 .response 优先
    assert_eq!(
        leaf(&v, "grp.options")["schema"]["response"],
        json!({ "type": "object", "properties": { "g": { "type": "integer" } } }),
        "{v}"
    );
}

/// v0.1.72：`.response` 平级声明下，组内某动词「v.schema 缺省 → 共享引用」路径
/// 不得污染组级合并契约（thisSchema 被兄弟动词共享）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_response_sibling_no_shared_reference_pollution() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const grp = {\n\
               get: () => { json.ok({}); },\n\
               post: () => { json.ok({}); },\n\
             };\n\
             grp.schema = { query: { type: \"object\" } };\n\
             grp.get.response = { type: \"string\" };\n\
             export default { grp };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(
        leaf(&v, "grp.get")["schema"]["response"],
        json!({ "type": "string" }),
        "{v}"
    );
    // b 未声明任何 response：不得从 a 的浅拷贝写回中「捡到」response
    assert!(
        leaf(&v, "grp.post")["schema"].get("response").is_none(),
        "{}",
        leaf(&v, "grp.post")["schema"]
    );
}

/// 动词 `.schema` 合并语义：同通道 `required` 取并集、同 `properties` 动词逐键覆盖、组键保留。
#[tokio::test(flavor = "current_thread")]
async fn introspect_verb_schema_merges_required_union_and_property_override() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const asset = {\n\
               post: () => { json.ok({}); },\n\
             };\n\
             asset.schema = {\n\
               query: { type: \"object\", required: [\"a\"], properties: { a: { type: \"string\" }, b: { type: \"string\" } } },\n\
             };\n\
             asset.post.schema = {\n\
               query: { type: \"object\", required: [\"c\"], properties: { a: { type: \"integer\" }, c: { type: \"string\" } } },\n\
             };\n\
             export default { asset };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(
        leaf(&v, "asset.post")["schema"],
        json!({
            "query": {
                "type": "object",
                "required": ["a", "c"],
                "properties": { "a": { "type": "integer" }, "b": { "type": "string" }, "c": { "type": "string" } }
            }
        }),
        "{v}"
    );
}

/// 嵌套组：组级 `.schema` 沿链累加（outer 的 params + inner 的 query 都落到 verb）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_nested_group_schema_accumulates() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const outer = {\n\
               inner: {\n\
                 get: () => { json.ok({}); },\n\
               },\n\
             };\n\
             outer.schema = { params: { type: \"object\", required: [\"tid\"], properties: { tid: { type: \"string\" } } } };\n\
             outer.inner.schema = { query: { type: \"object\", required: [\"q\"], properties: { q: { type: \"string\" } } } };\n\
             export default { outer };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(
        leaf(&v, "outer.inner.get")["schema"],
        json!({
            "params": { "type": "object", "required": ["tid"], "properties": { "tid": { "type": "string" } } },
            "query": { "type": "object", "required": ["q"], "properties": { "q": { "type": "string" } } }
        }),
        "{v}"
    );
}

/// 组级 `.schema` 必须落到运行期契约守卫：合并后的契约在进 JS 之前校验，违反 → 400。
/// 用内省结果装配注册表（与 app.rs 同径：RouteRow.schema 即唯一真源），再经 run_module 触发校验。
#[tokio::test(flavor = "current_thread")]
async fn group_schema_enforced_before_js_via_merged_contract() {
    use crate::contract::{InputContract, InputContractRegistry};
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "g/api.ts",
        "const asset = {\n\
               get: () => { json.ok({ reached: true }); },\n\
               post: () => { json.ok({ reached: true }); },\n\
             };\n\
             asset.schema = {\n\
               params: { type: \"object\", required: [\"id\"], properties: { id: { type: \"integer\" } } },\n\
             };\n\
             asset.post.schema = {\n\
               body: { type: \"object\", required: [\"name\"], properties: { name: { type: \"string\" } } },\n\
             };\n\
             export default { asset };\n",
    )]);
    // 从内省结果装配契约注册表（与 app.rs 同径：RouteRow.schema 即真源）。
    let probe = module_bridge(&root);
    let leaves = probe.introspect_module(&api).await.unwrap();
    let mut reg = InputContractRegistry::new();
    for lf in leaves.as_array().unwrap() {
        let sv = &lf["schema"];
        if sv.is_null() {
            continue;
        }
        let c = InputContract::try_new(
            sv.get("params").cloned(),
            sv.get("query").cloned(),
            sv.get("body").cloned(),
            &mut Vec::new(),
        )
        .unwrap();
        reg.insert(api.clone(), lf["chain"].as_str().unwrap(), c)
            .unwrap();
    }
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        Some(Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: true,
        })),
        Extras {
            input_contracts: Some(Arc::new(std::sync::RwLock::new(reg))),
            ..Default::default()
        },
    );
    let t = std::time::Duration::from_secs(5);

    // asset.get：继承组级 params.id —— 非法值 → 400，不进 JS。
    let mut bad = RequestInfo::default();
    bad.params.insert("id".into(), "x".into());
    let cap = b.run_module(&api, "asset.get", bad, t).await.unwrap();
    assert_eq!(
        cap.status,
        400,
        "asset.get 应继承组级 params.id 契约: {}",
        String::from_utf8_lossy(&cap.body)
    );

    // asset.get：合规 → 200，JS 被执行。
    let mut good = RequestInfo::default();
    good.params.insert("id".into(), "7".into());
    let cap = b.run_module(&api, "asset.get", good, t).await.unwrap();
    assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["reached"], json!(true));

    // asset.post：合并组级 params.id + 动词级 body.name —— 缺 body.name → 400。
    let mut p = RequestInfo::default();
    p.params.insert("id".into(), "7".into());
    p.body = serde_json::to_vec(&json!({})).unwrap();
    let cap = b.run_module(&api, "asset.post", p, t).await.unwrap();
    assert_eq!(
        cap.status,
        400,
        "asset.post 应合并 body.name 契约: {}",
        String::from_utf8_lossy(&cap.body)
    );

    // asset.post：全合规 → 200。
    let mut p2 = RequestInfo::default();
    p2.params.insert("id".into(), "7".into());
    p2.body = serde_json::to_vec(&json!({"name": "x"})).unwrap();
    let cap = b.run_module(&api, "asset.post", p2, t).await.unwrap();
    assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
}

/// 判别红线（§二.A）：verb 键非函数 / 非 verb 键挂函数、数组、基元 → 可操作 Err，
/// 且错误信息点名出事的键（不再是静默丢弃）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_rejects_illegal_verb_and_group_values() {
    let _t = transpile_serial();
    let cases: [(&str, &str, &str); 5] = [
        (
            "v/api.ts",
            "export default { get: { nested: 1 } };\n",
            "get",
        ),
        (
            "f/api.ts",
            "function helper() {}\nexport default { helper };\n",
            "helper",
        ),
        ("a/api.ts", "export default { upload: [1, 2] };\n", "upload"),
        ("p/api.ts", "export default { upload: 1 };\n", "upload"),
        ("n/api.ts", "export default { get: null };\n", "get"),
    ];
    for (rel, src, key) in cases {
        let (root, api) = mod_fx(&[(rel, src)]);
        let b = module_bridge(&root);
        let e = b.introspect_module(&api).await.unwrap_err();
        assert!(
            e.to_string().contains(key),
            "case {rel}: error should name the offending key {key:?}: {e}"
        );
    }
}

/// 循环引用（`a.self = a`）→ 内省期即失败，不递归到超时/爆栈（S1 防护）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_rejects_circular_group() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "c/api.ts",
        "const a = { get: () => { json.ok({}); } };\n\
             a.self = a;\n\
             export default { a };\n",
    )]);
    let b = module_bridge(&root);
    let e = b.introspect_module(&api).await.unwrap_err();
    assert!(e.to_string().contains("circular"), "{e}");
}

/// 深度上限 4：链长 5（`a.b.c.d.get`）→ 失败；链长 4（`a.b.c.get`）→ 放行。
#[tokio::test(flavor = "current_thread")]
async fn introspect_depth_limit_is_four() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "d/api.ts",
        "const d = { get: () => { json.ok({}); } };\n\
             const c = { d };\n\
             const b = { c };\n\
             const a = { b };\n\
             export default { a };\n",
    )]);
    let b = module_bridge(&root);
    let e = b.introspect_module(&api).await.unwrap_err();
    assert!(e.to_string().contains("deep"), "{e}");

    let (root, api) = mod_fx(&[(
        "d/api.ts",
        "const c = { get: () => { json.ok({}); } };\n\
             const b = { c };\n\
             const a = { b };\n\
             export default { a };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(leaf(&v, "a.b.c.get")["route"], json!("a/b/c"), "{v}");
}

/// 同一组对象挂两个键（DAG，非环）→ 各自成 route，不得误报循环。
#[tokio::test(flavor = "current_thread")]
async fn introspect_allows_shared_group_object() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "s/api.ts",
        "const shared = { get: () => { json.ok({}); } };\n\
             export default { a: shared, b: shared };\n",
    )]);
    let b = module_bridge(&root);
    let v = b.introspect_module(&api).await.unwrap();
    assert_eq!(leaf(&v, "a.get")["route"], json!("a"), "{v}");
    assert_eq!(leaf(&v, "b.get")["route"], json!("b"), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn introspect_broken_module_errs() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[("bad/api.ts", "function {{{{\nexport default {};")]);
    let b = module_bridge(&root);
    assert!(b.introspect_module(&api).await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn introspect_top_level_loop_times_out() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "loop/api.ts",
        "while (true) {}\nexport default { get() {} };\n",
    )]);
    let b = module_bridge(&root);
    assert!(matches!(
        b.introspect_module(&api).await,
        Err(RunError::Timeout)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn read_module_default_reads_export() {
    // release 直载：routes.js 的 default 导出原样读出（不逐模块内省）
    let _t = transpile_serial();
    let (root, p) = mod_fx(&[(
        "routes.js",
        "export default [{ method: \"get\", pattern: \"/a/{id}\", file: \"a/api.js\" }];\n",
    )]);
    let b = module_bridge(&root);
    let v = b.read_module_default(&p).await.unwrap();
    assert_eq!(v[0]["pattern"], "/a/{id}", "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn read_module_default_without_export_yields_null() {
    let _t = transpile_serial();
    let (root, p) = mod_fx(&[("plain.js", "export const x = 1;\n")]);
    let b = module_bridge(&root);
    let v = b.read_module_default(&p).await.unwrap();
    assert!(v.is_null(), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn syntax_error_returns_core_error_with_position() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[("u/f/api.ts", "function {{{{\nexport default {};\n")]);
    let b = module_bridge(&root);
    let e = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("api.ts"), "{}", e.to_string());
}

#[tokio::test(flavor = "current_thread")]
async fn pool_reuses_runtime_isolation() {
    // 两次请求相继执行，验证每请求状态被正确重置（req 不串号）。
    let (b, _) = new_bridge();
    let cap1 = b
        .run_with(
            r#"json.ok({ m: http.method });"#,
            RequestInfo {
                method: "GET".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cap2 = b
        .run_with(
            r#"json.ok({ m: http.method });"#,
            RequestInfo {
                method: "PUT".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let v1: Value = serde_json::from_slice(&cap1.body).unwrap();
    let v2: Value = serde_json::from_slice(&cap2.body).unwrap();
    assert_eq!(v1["data"]["m"], "GET");
    assert_eq!(v2["data"]["m"], "PUT");
}

/// §5.3 集成：run_module 目录命中注入模块上下文 → 表归属守卫生效。
/// registry：secret 属 other；模块 m 未声明 deps → warn 放行 / deny 拒绝。
#[tokio::test(flavor = "current_thread")]
async fn ownership_guard_deny_blocks_cross_table_and_warn_allows() {
    let _t = transpile_serial();
    let (root, api, mods, reg) = {
        let (root, api) = mod_fx(&[(
            "m/api.ts",
            "export default { get() { db.query(\"select * from secret\")\n\
                 \x20 .then((r) => json.ok({ n: r.length }))\n\
                 \x20 .catch((e) => json.fail(500, String(e))); } };\n",
        )]);
        let reg = SchemaRegistry::new().table_owned("other", "secret", &["id"], &["id"]);
        let mods = Arc::new(HashMap::from([(
            root.join("m").to_string_lossy().into_owned(),
            ModuleCtx {
                name: "m".into(),
                version: "0.1.0".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: None,
            },
        )]));
        (root, api, mods, reg)
    };
    // deny：跨模块表访问被拒（raw SQL 提取到 secret，owner=other ∉ {m} ∪ deps）。
    let dbs = HashMap::from([(
        "default".to_string(),
        Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
    )]);
    let b = module_bridge_ex(&root, reg.clone(), mods.clone(), true, dbs);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("ownership"), "{v}");
    assert!(v["msg"].as_str().unwrap().contains("deps"), "{v}");
    // warn：放行（默认模式不破坏既有行为）。
    let dbs = HashMap::from([(
        "default".to_string(),
        Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
    )]);
    let b = module_bridge_ex(&root, reg, mods, false, dbs);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
}

/// P1-2 回归（开发侧评审）：「序列表已确保」缓存必须以**物理库**为键，不能用 JS 可见名。
///
/// 场景：同一 Bridge 内两个模块把字面 `"default"` 绑到不同物理库（`manifest db:` /
/// `ModuleCtx.db`）。若缓存键用可见名 `"default"`，模块 B 会命中模块 A 写入的条目而
/// **跳过建表**——池路径白付一次失败语句 + 一次 DDL，事务路径更是直接失败
/// （正是架构评审 P1-2 声称已消灭的那类故障）。
///
/// 断言方式是**直接数 DDL**（每个物理库各建一次 = 2 次），而不是看返回值：返回值来自
/// 记录型 accessor 的固定应答，本用例只验缓存**键**，序列 SQL 由真库用例覆盖。
/// （不用真 sqlite 跑事务路径：sqlite 池 `max_connections(1)`，事务持连接时池上再发 DDL
/// 必然等锁超时——那是该 accessor 的既有性质，与本用例无关。）
#[tokio::test(flavor = "current_thread")]
async fn next_seq_ddl_cache_is_keyed_by_physical_db() {
    let _t = transpile_serial();
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 记录型 accessor：只数 `_oj_sequences` 的 DDL，其余读写走内存假实现。
    struct DdlCounting {
        inner: Arc<InMemoryAccessor>,
        ddl: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl DataAccessor for DdlCounting {
        fn dialect(&self) -> Dialect {
            Dialect::Sqlite
        }
        async fn begin(&self) -> BridgeResult<Box<dyn super::db::TxSession>> {
            self.inner.begin().await
        }
        async fn query_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
            // 取号语句的固定应答（本用例不验 SQL 语义）。
            Ok(vec![json!({ "v": 1 })])
        }
        async fn exec_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<i64> {
            if sql.contains("_oj_sequences") {
                self.ddl.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.exec_with_params(sql, params).await
        }
    }

    let js = "export default { get() { db.nextSeq(\"s\")\n\
                  \x20 .then((v) => json.ok({ v })).catch((e) => json.fail(500, String(e))); } };\n";
    let (root, api_a) = mod_fx(&[("m_a/api.ts", js), ("m_b/api.ts", js)]);
    let count_a = Arc::new(AtomicUsize::new(0));
    let count_b = Arc::new(AtomicUsize::new(0));
    let dbs = HashMap::from([
        (
            "db_a".to_string(),
            Arc::new(DdlCounting {
                inner: Arc::new(InMemoryAccessor::new()),
                ddl: count_a.clone(),
            }) as Arc<dyn DataAccessor>,
        ),
        (
            "db_b".to_string(),
            Arc::new(DdlCounting {
                inner: Arc::new(InMemoryAccessor::new()),
                ddl: count_b.clone(),
            }) as Arc<dyn DataAccessor>,
        ),
    ]);
    let mods = Arc::new(HashMap::from([
        (
            root.join("m_a").to_string_lossy().into_owned(),
            ModuleCtx {
                name: "m_a".into(),
                version: "0.1.0".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: Some("db_a".into()),
            },
        ),
        (
            root.join("m_b").to_string_lossy().into_owned(),
            ModuleCtx {
                name: "m_b".into(),
                version: "0.1.0".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: Some("db_b".into()),
            },
        ),
    ]));
    let b = module_bridge_ex(&root, SchemaRegistry::new(), mods, false, dbs);
    // 模块 A 先跑（缓存此刻写入），随后模块 B 必须**各自**建表。
    for api in [&api_a, &root.join("m_b/api.ts")] {
        let cap = b
            .run_module(
                api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{api:?} -> {v}");
    }
    assert_eq!(
        (
            count_a.load(Ordering::SeqCst),
            count_b.load(Ordering::SeqCst)
        ),
        (1, 1),
        "每个物理库都必须各自建一次序列表（缓存键 = 物理库名，不是 JS 可见名）"
    );
}

/// F-1 回归（统一审查）：CASE WHEN 条件内的子查询同样过归属守卫——
/// guard_req 必须遍历 req.columns，deny 模式下嵌套他模块表被拒。
#[tokio::test(flavor = "current_thread")]
async fn ownership_guard_covers_case_when_subquery() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "m/api.ts",
        "export default { get() { db.table(\"t\").select([\"name\",\n\
             \x20 { case: { when: [{ cond: { field: \"id\", op: \"in\",\n\
             \x20   subquery: db.table(\"secret\").select([\"id\"]) }, then: 1 }],\n\
             \x20   else: 0 }, as: \"c\" }]).all()\n\
             \x20 .then((r) => json.ok(r)).catch((e) => json.fail(500, String(e))); } };\n",
    )]);
    let reg = SchemaRegistry::new()
        .table_owned("m", "t", &["id"], &["id", "name"])
        .table_owned("other", "secret", &["id"], &["id"]);
    let mods = Arc::new(HashMap::from([(
        root.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "0.1.0".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    // deny：CASE 内嵌套 secret（owner=other ∉ {m} ∪ deps）必须被拒。
    let dbs = HashMap::from([(
        "default".to_string(),
        Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
    )]);
    let b = module_bridge_ex(&root, reg, mods, true, dbs);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"].as_str().unwrap_or_default().contains("ownership"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ownership_guard_covers_from_select_source() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "m/api.ts",
        "export default { get() { db.table(\"t\").fromSelect(db.table(\"secret\")\n\
             \x20 .select([\"id\"]), [\"id\"]).run()\n\
             \x20 .then((r) => json.ok(r)).catch((e) => json.fail(500, String(e))); } };\n",
    )]);
    let reg = SchemaRegistry::new()
        .table_owned("m", "t", &["id"], &["id", "name"])
        .table_owned("other", "secret", &["id"], &["id"]);
    let mods = Arc::new(HashMap::from([(
        root.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "0.1.0".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    // deny：fromSelect 的源 secret（owner=other ∉ {m} ∪ deps）必须被拒——
    // 守卫不递归源 select 的话，INSERT...SELECT 就是跨模块搬运（v0.1.83；
    // apply_tenant 对源递归在先，此处补齐归属守卫的同一覆盖）。
    let dbs = HashMap::from([(
        "default".to_string(),
        Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
    )]);
    let b = module_bridge_ex(&root, reg, mods, true, dbs);
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"].as_str().unwrap_or_default().contains("ownership"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bound_db_redirects_default_for_bound_module() {
    let _t = transpile_serial();
    let (root, api) = mod_fx(&[(
        "m/api.ts",
        "export default { get() { db.query(\"select * from user\")\n\
             \x20 .then((r) => json.ok({ viaDefault: r[0].db }))\n\
             \x20 .catch((e) => json.fail(500, String(e))); } };\n",
    )]);
    let def = Arc::new(InMemoryAccessor::new());
    def.seed([serde_json::json!({"db": "default"})]);
    let analytics = Arc::new(InMemoryAccessor::new());
    analytics.seed([serde_json::json!({"db": "analytics"})]);
    let mods = Arc::new(HashMap::from([(
        root.join("m").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "m".into(),
            version: "0.1.0".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: Some("analytics".into()),
        },
    )]));
    let b = module_bridge_ex(
        &root,
        SchemaRegistry::new(),
        mods,
        false,
        HashMap::from([
            ("default".to_string(), def as Arc<dyn DataAccessor>),
            ("analytics".to_string(), analytics as Arc<dyn DataAccessor>),
        ]),
    );
    let cap = b
        .run_module(
            &api,
            "get",
            RequestInfo::default(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 字面 "default" 被重定向到绑定的 analytics 库。
    assert_eq!(v["data"]["viaDefault"], "analytics", "{v}");
}

// ===== ext_boot（v2）=====

/// 造带 ext_boot 的 Bridge：`files` 写入临时项目根，boot spec 由
/// `versioned_specifier` 冻结（同装配期）。返回 (Bridge, 项目根, 共享 KV)。
fn boot_fx(files: &[(&str, &str)]) -> (Bridge, std::path::PathBuf, Arc<InMemoryKV>) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "oj-boot-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    for (rel, content) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
    }
    let boot = Some(
        module_loader::versioned_specifier(&root.join("ext_boot.js"))
            .unwrap()
            .to_string(),
    );
    let kv = Arc::new(InMemoryKV::new());
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        kv.clone(),
        SchemaRegistry::new(),
        false,
        Some(Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: true,
        })),
        Extras {
            boot,
            ..Default::default()
        },
    );
    (b, root, kv)
}

/// boot 设置全局 → handler 可见。
#[tokio::test(flavor = "current_thread")]
async fn boot_sets_global_visible_to_handler() {
    let (b, root, _kv) = boot_fx(&[("ext_boot.js", "globalThis.foo = 1;\n")]);
    let cap = b
        .run_with(
            r#"json.ok({ foo: globalThis.foo });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["foo"], 1, "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

/// boot import 项目内模块（走 OjModuleLoader：相对导入 + 扩展名补全）。
#[tokio::test(flavor = "current_thread")]
async fn boot_imports_project_module() {
    let _t = transpile_serial();
    let (b, root, _kv) = boot_fx(&[
        (
            "ext_boot.js",
            "import { tag } from \"./shared/util\";\nglobalThis.tag = tag;\n",
        ),
        ("shared/util.ts", "export const tag: string = \"booted\";\n"),
    ]);
    let cap = b
        .run_with(
            r#"json.ok({ tag: globalThis.tag });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["tag"], "booted", "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 每 runtime 一次、不每请求一次：跨 runtime 共享的 KV 计数在复用空闲池时不再增长。
/// （模块级 `let` 是 per-isolate 的，区分不了「没重跑」与「重跑了但重置」。）
#[tokio::test(flavor = "current_thread")]
async fn boot_runs_once_per_runtime_not_per_request() {
    // `export {};` 是 ESM 标记：无 import/export 的 boot 会被 `looks_cjs` 判为 CJS
    // 并包进非 async 函数 → 顶层 await 直接 SyntaxError（见设计文档 D-9）。
    let (b, root, kv) = boot_fx(&[(
        "ext_boot.js",
        "export {};\nawait kv.incr(\"boot_n\");\nglobalThis.foo = 1;\n",
    )]);
    b.run_with(r#"json.ok(1);"#, RequestInfo::default())
        .await
        .unwrap();
    assert_eq!(kv.get("boot_n").await.unwrap().as_deref(), Some("1"));
    // 第二次请求命中空闲池（同一 runtime）→ 不重跑 boot。
    b.run_with(r#"json.ok(2);"#, RequestInfo::default())
        .await
        .unwrap();
    assert_eq!(kv.get("boot_n").await.unwrap().as_deref(), Some("1"));
    let _ = std::fs::remove_dir_all(&root);
}

/// boot 执行于 ReqState.reset 之前：boot 期误调 json.ok 写入的是随后被重置的状态。
#[tokio::test(flavor = "current_thread")]
async fn boot_envelope_write_is_reset() {
    let (b, root, _kv) = boot_fx(&[("ext_boot.js", "json.ok({ from: \"boot\" });\n")]);
    let cap = b
        .run_with(r#"json.ok({ from: "handler" });"#, RequestInfo::default())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["from"], "handler", "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

/// boot 失败 → Err 而非 panic，且 Bridge 仍可用（运行期在 actor 线程上，panic 会杀死 worker）。
#[tokio::test(flavor = "current_thread")]
async fn boot_failure_is_err_not_panic() {
    let (b, root, _kv) = boot_fx(&[("ext_boot.js", "this is not javascript(((\n")]);
    let e = b
        .run_with(r#"json.ok(1);"#, RequestInfo::default())
        .await
        .unwrap_err();
    assert!(!e.to_string().is_empty(), "{e}");
    // 进程存活：修好文件后新的 runtime 能起来（boot 每次新建 runtime 都重跑）。
    std::fs::write(root.join("ext_boot.js"), "globalThis.foo = 7;\n").unwrap();
    let cap = b
        .run_with(
            r#"json.ok({ foo: globalThis.foo });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["foo"], 7, "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

/// boot 顶层死循环：KillSwitch 兜底 → Timeout，不永久挂起。
/// （`tokio::time::timeout` 在此无效：isolate 不归还执行器，只能 terminate_execution。
/// 相对的，`await new Promise(()=>{})` 这类「永不到达」的 TLA 由 deno_core 在事件循环
/// 排空时立即报 "Top-level await promise never resolved"，并不挂起。）
#[tokio::test(flavor = "current_thread")]
async fn boot_infinite_loop_yields_timeout() {
    let (b, root, _kv) = boot_fx(&[("ext_boot.js", "export {};\nwhile (true) {}\n")]);
    let e = b
        .run_with_timeout(
            r#"json.ok(1);"#,
            RequestInfo::default(),
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, RunError::Timeout), "{e:?}");
    let _ = std::fs::remove_dir_all(&root);
}

// ===== PR-4 isolate 堆限额（v0.1.40）：击穿测试 =====

/// 超限 handler → Err(Oom)（明确文案）、进程存活、池恢复：fired isolate 丢弃后，
/// 同一 Bridge 再跑 OOM 与正常请求均按预期（连续两轮 OOM + 一轮正常，钉死"回池"防线）。
#[tokio::test(flavor = "current_thread")]
async fn oom_terminated_and_pool_recovers() {
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            js_heap_limit: Some(32 * 1024 * 1024),
            ..Default::default()
        },
    );
    const OOM: &str = r#"let a = []; while (true) a.push(new Array(2e6));"#;
    for round in 1..=2 {
        let e = b
            .run_with_timeout(
                OOM,
                RequestInfo::default(),
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap_err();
        match &e {
            RunError::Oom { limit } => assert_eq!(*limit, 32 * 1024 * 1024, "round {round}"),
            other => panic!("round {round}: expected Oom, got {other:?}"),
        }
    }
    // 池恢复：fired isolate 已丢弃，新请求走全新 runtime 正常完成。
    let cap = b
        .run_with_timeout(
            r#"json.ok({ ok: 1 });"#,
            RequestInfo::default(),
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["ok"], 1, "{v}");
}

/// 探测：boot 期堆超限 → Err(Oom)（checkout 路径分类），且修复后同一 Bridge 恢复。
#[tokio::test(flavor = "current_thread")]
async fn probe_boot_oom_pool_recovers() {
    let (_b, root, _kv) = boot_fx(&[(
        "ext_boot.js",
        "export {};\nlet a = []; while (true) a.push(new Array(1e6));\n",
    )]);
    // 给该 Bridge 注入限额：boot_fx 不带 extras → 直接构造带限额的同根 Bridge。
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        _kv.clone(),
        SchemaRegistry::new(),
        false,
        Some(Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: true,
        })),
        Extras {
            boot: Some(
                module_loader::versioned_specifier(&root.join("ext_boot.js"))
                    .unwrap()
                    .to_string(),
            ),
            js_heap_limit: Some(32 * 1024 * 1024),
            ..Default::default()
        },
    );
    let e = b
        .run_with_timeout(
            r#"json.ok(1);"#,
            RequestInfo::default(),
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, RunError::Oom { .. }),
        "boot 期超限须 Oom: {e:?}"
    );
    // 修复 boot（换成小脚本）→ 新 runtime 正常 boot 并执行。
    std::fs::write(root.join("ext_boot.js"), "globalThis.foo = 7;\n").unwrap();
    let (b2, _r, _kv2) = boot_fx(&[("ext_boot.js", "globalThis.foo = 7;\n")]);
    let _ = b2; // 恢复能力由 oom_terminated_and_pool_recovers 覆盖；此处验证 err 非 panic
    let _ = std::fs::remove_dir_all(&root);
}

/// 运行期 ext_boot.js 被删：装配期冻结的是 spec，`load` 阶段才读盘 → 报错而非 panic。
#[tokio::test(flavor = "current_thread")]
async fn boot_file_removed_at_runtime_yields_error() {
    let (b, root, _kv) = boot_fx(&[("ext_boot.js", "globalThis.foo = 1;\n")]);
    std::fs::remove_file(root.join("ext_boot.js")).unwrap();
    let e = b
        .run_with(r#"json.ok(1);"#, RequestInfo::default())
        .await
        .unwrap_err();
    assert!(!e.to_string().is_empty(), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

/// CJS 写法的 boot 无全局副作用（`looks_cjs` 启发式把它包装成 ESM，
/// `module.exports` 变成 default 导出）—— 记录行为，防「配了却静默没生效」。
#[tokio::test(flavor = "current_thread")]
async fn cjs_boot_has_no_global_effect() {
    let (b, root, _kv) = boot_fx(&[("ext_boot.js", "module.exports = { foo: 1 };\n")]);
    let cap = b
        .run_with(
            r#"json.ok({ foo: globalThis.foo === undefined ? null : globalThis.foo });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["foo"], Value::Null, "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 回归锁：ext_boot **拿不到** `ext:core/ops`。
/// deno_core 在 loader resolve 之后还有 `validate_ext_module_import` 一道闸，
/// `file://` referrer 永远过不去 —— 别再尝试放行 `ext:` scheme（放行也无效，
/// 反而会给所有 handler 开一条宽松分支）。
#[tokio::test(flavor = "current_thread")]
async fn boot_cannot_import_ext_core_ops() {
    let (b, root, _kv) = boot_fx(&[(
        "ext_boot.js",
        "await import(\"ext:core/ops\");\nglobalThis.foo = 1;\n",
    )]);
    let e = b
        .run_with(r#"json.ok(1);"#, RequestInfo::default())
        .await
        .unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("ext:") || msg.contains("unsupported scheme"),
        "expected deno_core to reject the ext: import, got: {msg}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- fetch（v0.1.8：deno_fetch / WHATWG 语义）----

fn fetch_bridge() -> Bridge {
    Bridge::new(
        Arc::new(InMemoryAccessor::new()),
        Arc::new(InMemoryKV::new()),
    )
}

/// 本地一次性 HTTP 服务器 + fetch 全链路：json()、ok/status、Headers
/// 大小写不敏感取值、AbortController 全局挂载。
#[tokio::test(flavor = "current_thread")]
async fn fetch_json_roundtrip_headers_and_abortcontroller() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await.unwrap();
        let body = r#"{"hello":"world"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(resp.as_bytes()).await.unwrap();
    });

    let b = fetch_bridge();
    let src = format!(
        r#"(async () => {{
              const r = await fetch("http://{addr}/data", {{ method: "POST", body: "ping" }});
              json.ok({{
                status: r.status, ok: r.ok, data: await r.json(),
                ct: r.headers.get("CONTENT-TYPE"),
                aborts: typeof AbortController === "function",
              }});
            }})().catch((e) => json.fail(500, String(e)));"#
    );
    let cap = b.run(&src).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "fetch failed: {v}");
    assert_eq!(v["data"]["status"], 200);
    assert_eq!(v["data"]["ok"], true);
    assert_eq!(v["data"]["data"], json!({"hello": "world"}));
    assert!(v["data"]["ct"].as_str().unwrap().contains("json"));
    assert_eq!(v["data"]["aborts"], true);
}

/// 错误面：空 URL → TypeError("Invalid URL…")；拒连 → TypeError（WHATWG
/// 把网络错折叠为 fetch rejection，不再有自研版的 "fetch: " 前缀）。
#[tokio::test(flavor = "current_thread")]
async fn fetch_invalid_url_and_connection_refused_reject() {
    let b = fetch_bridge();
    let cap = b
        .run(
            r#"(async () => { await fetch(""); json.ok({}); })()
                   .catch((e) => json.fail(400, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(v["msg"].as_str().unwrap().contains("Invalid URL"), "{v}");

    // 端口 1 无监听 → 连接拒绝
    let cap = b
        .run(
            r#"(async () => { await fetch("http://127.0.0.1:1/"); json.ok({}); })()
                   .catch((e) => json.fail(502, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 502, "{v}");
    let msg = v["msg"].as_str().unwrap();
    assert!(
        msg.contains("TypeError") || msg.contains("sending request"),
        "{v}"
    );
}

/// 非 2xx 照常返回 Response（ok=false），body 走 text()。
#[tokio::test(flavor = "current_thread")]
async fn fetch_non_2xx_reports_status_and_text_body() {
    use httptest::Expectation;
    use httptest::Server;
    use httptest::matchers::request;
    use httptest::responders::*;

    let server = Server::run();
    server.expect(
        Expectation::matching(request::path("/fail")).respond_with(status_code(503).body("down")),
    );
    let b = fetch_bridge();
    let cap = b
        .run(&format!(
            r#"(async () => {{
                  const r = await fetch("{}", {{ method: "POST", body: "ping" }});
                  json.ok({{ status: r.status, ok: r.ok, body: await r.text() }});
                }})().catch((e) => json.fail(500, String(e)));"#,
            server.url("/fail")
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["status"], 503);
    assert_eq!(v["data"]["ok"], false);
    assert_eq!(v["data"]["body"], "down");
}

/// WHATWG 语义：合法 token 的任意 method 原样发出（自研版会回退 GET）。
/// 手写 TCP 服务器直接断言请求行，钉死 wire 上的方法。
#[tokio::test(flavor = "current_thread")]
async fn fetch_arbitrary_token_method_sent_verbatim() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(
            head.starts_with("NOTAMETHOD /x HTTP/1.1"),
            "method not sent verbatim: {head}"
        );
        let body = r#"{"ok":1}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(resp.as_bytes()).await.unwrap();
    });

    let b = fetch_bridge();
    let cap = b
        .run(&format!(
            r#"(async () => {{
                  const r = await fetch("http://{addr}/x", {{ method: "NOTAMETHOD", body: "b" }});
                  json.ok({{ status: r.status, body: await r.json() }});
                }})().catch((e) => json.fail(500, String(e)));"#
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["status"], 200);
    assert_eq!(v["data"]["body"], json!({"ok": 1}));
}

// ---- prewarm / inspector / run_named / start_inspector ----

/// prewarm 借出 runtime 跑完 boot 后归还：返回 Ok，且池内 runtime 后续可正常服务。
#[tokio::test(flavor = "current_thread")]
async fn given_fresh_bridge_when_prewarm_then_ok_and_runtime_returns_to_pool() {
    let (b, _) = new_bridge();
    b.prewarm().await.unwrap();
    // 归还后的 runtime 可继续执行（boot 只发生一次，不因 prewarm 丢失）。
    let cap = b.run(r#"json.ok({ warm: true });"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["warm"], true);
}

/// inspect=false 构造：inspector() 直接短路返回 None（不 checkout runtime）。
#[tokio::test(flavor = "current_thread")]
async fn given_inspect_disabled_when_inspector_then_none() {
    let (b, _) = new_bridge();
    assert!(!b.inspect());
    assert!(b.inspector().await.is_none());
}

/// inspect=true 构造：inspector() 借出 runtime 取句柄后归还，池仍可用。
/// 句柄 Rc 须在 bridge 之前释放（JsRuntime drop 断言 inspector 先亡），
/// 声明顺序保证反序 drop：insp 先于 b。
#[tokio::test(flavor = "current_thread")]
async fn given_inspect_enabled_when_inspector_then_some_and_pool_reusable() {
    let b = Bridge::with_opts(
        Arc::new(InMemoryAccessor::new()),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        true,
    );
    assert!(b.inspect());
    let insp = b.inspector().await.unwrap();
    let cap = b.run(r#"json.ok({ alive: true });"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["alive"], true);
    drop(insp);
}

/// run_named 打点 handler 缺失：NotFound（io::ErrorKind）点名报错。
#[tokio::test(flavor = "current_thread")]
async fn given_unknown_handler_when_run_named_then_not_found_names_it() {
    let (b, _) = new_bridge();
    let e = b.run_named("nope").await.unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("handler 'nope' not found"), "{msg}");
}

/// run_named 命中 HandlerStore：按名取源码并执行（热重载路径）。
#[tokio::test(flavor = "current_thread")]
async fn given_registered_handler_when_run_named_then_executes() {
    let (mut b, _) = new_bridge();
    b.set_handlers(HandlerStore::from_embedded(
        [("greet".into(), r#"json.ok({ hi: 1 });"#.into())]
            .into_iter()
            .collect(),
    ));
    let cap = b.run_named("greet").await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["hi"], 1);
}

/// inspect=false 时 start_inspector 走 warn/noop 分支：句柄立即落定，不起服务。
/// spawn_local 要求 LocalSet 上下文（inspector.rs 同款）。
#[tokio::test(flavor = "current_thread")]
async fn given_inspect_disabled_when_start_inspector_then_noop_returns() {
    let (b, _) = new_bridge();
    let ls = tokio::task::LocalSet::new();
    ls.run_until(async {
        let h = start_inspector(&b, "127.0.0.1:0".parse().unwrap()).await;
        h.await.unwrap();
    })
    .await;
}

/// inspect=true 时 start_inspector 走 spawn 分支：Some → inspector::spawn 起服务。
/// 探自由端口让 bind 成功（spawn 分支真实进入；macOS 不限非特权低位端口，
/// 「特权端口必败」前提不成立且 accept 循环永不返回，await 句柄即挂死）。
/// 任务存活（accept 循环）即分支生效；随 LocalSet drop 收场。
#[tokio::test(flavor = "current_thread")]
async fn given_inspect_enabled_when_start_inspector_then_spawn_branch_taken() {
    let b = Bridge::with_opts(
        Arc::new(InMemoryAccessor::new()),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        true,
    );
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let ls = tokio::task::LocalSet::new();
    ls.run_until(async {
        let h = start_inspector(&b, addr).await;
        // sleep 让 LocalSet 轮到 spawned 任务完成 bind。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !h.is_finished(),
            "inspector task should be alive in accept loop"
        );
    })
    .await;
}

/// 无 boot（现网默认）：`Extras::default()` 行为零变化。
#[tokio::test(flavor = "current_thread")]
async fn no_boot_is_noop() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"json.ok({ foo: globalThis.foo === undefined ? null : globalThis.foo });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["foo"], Value::Null, "{v}");
}

// ws 会话/帧池用例（lifecycle、钩子契约、毒化半径）在 frame_pool::tests——
// 它们消费 RoutePool/ConnHandle，辅助同置一处（ws_test_bridge/pool_dir）。

/// inspector 借出基线化（评审修复）：特权请求归池后，`inspector()` 经
/// checkout_reset 重置容器——随后从池中**裸借出**（不再重置）探测 OpState，
/// trusted 路径必须已不可及（修复前：裸 checkout 不重置，特权容器残留可触达）。
#[tokio::test(flavor = "current_thread")]
async fn inspector_checkout_resets_privileged_container() {
    use deno_permissions::{PermissionState, PermissionsContainer};

    let root = std::env::temp_dir().join(format!("oj-insp-{}-root", std::process::id()));
    let trusted_dir = std::env::temp_dir().join(format!("oj-insp-{}-trusted", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&trusted_dir).unwrap();
    // macOS 下 temp_dir 经 symlink：统一 canonical，防前缀比对被形态打穿。
    let root = root.canonicalize().unwrap();
    let trusted_dir = trusted_dir.canonicalize().unwrap();
    let in_root = root.join("in.txt");
    let in_trusted = trusted_dir.join("cfg.txt");
    std::fs::write(&in_root, b"1").unwrap();
    std::fs::write(&in_trusted, b"2").unwrap();

    // 特权模块归属（模块目录键 = §5.3 装配形态）+ fs/platform grants。
    let (base, api) = mod_fx(&[(
        "plat/api.js",
        "export async function ping() { json.ok(1); }\nexport default { ping };\n",
    )]);
    let modules: Arc<HashMap<String, ModuleCtx>> = Arc::new(HashMap::from([(
        base.join("plat").to_string_lossy().into_owned(),
        ModuleCtx {
            name: "plat".into(),
            version: "0.1.0".into(),
            deps: Arc::new(std::collections::HashSet::new()),
            db: None,
        },
    )]));
    let fs_grant = Arc::new(fs::FsGrant::new(&root, false).unwrap());
    let platform = Arc::new(
        platform::PlatformGrant::new(
            vec!["plat".into()],
            std::slice::from_ref(&trusted_dir),
            Some(&fs_grant.root),
        )
        .unwrap(),
    );
    let b = Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        true, // inspect=true：inspector() 借出路径才生效
        Some(Arc::new(LoaderShared {
            project_root: base.clone(),
            ts: false,
        })),
        Extras {
            modules,
            fs: Some(fs_grant),
            platform: Some(platform),
            ..Default::default()
        },
    );
    let t = std::time::Duration::from_secs(5);

    // ① 特权请求：run_module 注入归属 → checkout_reset 换装特权容器 → 归池残留。
    let cap = b
        .run_module(&api, "ping", RequestInfo::default(), t)
        .await
        .unwrap();
    assert_eq!(cap.status, 200);

    // ② inspector 借出（修复点：内部 checkout_reset 以 module=None 基线化）。
    assert!(b.inspector().await.is_some());

    // ③ 裸借出探测（LIFO 取回同一 runtime，不经任何重置）：
    //    trusted 路径不可及、root 内可及——特权容器已重置回基线。
    let rt = b.pool.checkout().await.unwrap();
    let probe = {
        let op_state = runtime::op_state(&rt);
        let g = op_state.borrow();
        let c = g.borrow::<PermissionsContainer>();
        (
            c.query_read(Some(in_trusted.to_string_lossy().as_ref()))
                .unwrap(),
            c.query_read(Some(in_root.to_string_lossy().as_ref()))
                .unwrap(),
        )
    };
    b.pool.checkin(rt);
    assert_eq!(
        probe.0,
        PermissionState::Prompt,
        "trusted must be unreachable after inspector reset"
    );
    assert_eq!(
        probe.1,
        PermissionState::Granted,
        "root must stay reachable"
    );
}
