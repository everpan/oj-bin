//! E2E（svc 凭据线，spec 2026-11-11 P1-P3）：description 透传 / scopes 三档 /
//! obo 换签全矩阵。
//!
//! 独立测试二进制的原因见 common/mod.rs 头注：插件每进程单例（oj-auth GUARD
//! 首次 init 后忽略后续 cfg），本二进制内**所有** boot 都带同一份 auth cfg
//! （SVC_SECRET）——首个 boot 的 init 即定型，后续 boot 同秘钥零冲突。跑本
//! 二进制要求 bin/plugins 先归置（CI release.yml/plugin-matrix.yml 同序：
//! `cargo xtask build` 先于 cargo test）。

// lock() 的 std MutexGuard 有意全程持有（用例串行），横跨 await 是设计而非疏漏。
#![allow(clippy::await_holding_lock)]

use std::path::Path;

use serde_json::{Value, json};

mod common;
use common::{boot_inner, lock};

const SVC_SECRET: &str = "e2e-svc-secret";

/// 本二进制统一引导：sample 真实链路 + oj-auth 守卫（秘钥 SVC_SECRET）+
/// mcp/rpc 双端点开 + 独立 sqlite。tweak 叠加在 auth/端点配置之后。
async fn boot_svc(
    tweak: impl FnOnce(&mut only_js::config::Config),
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    boot_inner(true, |c| {
        c.auth = Some(only_js::config::AuthCfg {
            jwt_secret: SVC_SECRET.into(),
            anonymous_paths: vec![
                only_js::config::AnonPath::Plain("/auth/login".into()),
                only_js::config::AnonPath::Plain("/auth/refresh".into()),
                only_js::config::AnonPath::Plain("/auth/logout".into()),
            ],
            ..Default::default()
        });
        c.server.mcp = Some(only_js::config::McpCfg {
            enabled: true,
            ..Default::default()
        });
        c.server.rpc.enabled = true;
        tweak(c);
    })
    .await
    .unwrap()
}

fn sign_hs256(secret: &str, claims: Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

/// svc 凭据（P3 形状：roles ["bot"] + claims svc/jti/scopes 平铺）。
fn svc_token(scopes: &[&str], extra: Value) -> String {
    let now = serve::test_support::now_secs();
    let mut claims = json!({
        "sub": "svc:e2e", "roles": ["bot"], "iat": now, "exp": now + 3600,
        "svc": "e2e", "jti": "jt-e2e", "scopes": scopes,
    });
    if let (Some(a), Some(b)) = (claims.as_object_mut(), extra.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
    sign_hs256(SVC_SECRET, claims)
}

fn admin_token(extra: Value) -> String {
    let now = serve::test_support::now_secs();
    let mut claims = json!({
        "sub": "1", "roles": ["admin"], "iat": now, "exp": now + 3600,
    });
    if let (Some(a), Some(b)) = (claims.as_object_mut(), extra.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
    sign_hs256(SVC_SECRET, claims)
}

/// rpc_req 的带凭据变体（P3：tools/list 裁剪与 admit 都要 Bearer）。
async fn rpc_req_auth(
    addr: std::net::SocketAddr,
    path: &str,
    body: &str,
    token: &str,
) -> (u16, Value) {
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    // token 空串 = 完全不带鉴权头（裸 "Bearer " 会被守卫当坏 token 拒，
    // anonymous 兜底只在头缺席时生效）。
    let mut r = c
        .post(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if !token.is_empty() {
        r = r.header("authorization", format!("Bearer {token}"));
    }
    let resp = r.body(body.to_string()).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

async fn req_auth(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: &str,
) -> (u16, Value) {
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let mut r = c
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{addr}{path}"),
        )
        .header("authorization", format!("Bearer {token}"));
    if let Some(b) = body {
        r = r
            .header("content-type", "application/json")
            .body(b.to_string());
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// 直接操作 tmp sqlite（播 jti 行 / users.disabled——业务面外的最小夹具）。
async fn db_exec(file: &Path, sql: &str) {
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(file);
    let db = only_js::bridge::SqlxAccessor::arc(&dsn).await.unwrap();
    db.exec_with_params(sql, &[]).await.unwrap();
}

fn has_svc_token_table(t: &Path) -> impl std::future::Future<Output = bool> + '_ {
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&t.join("db.sqlite"));
    async move {
        let db = only_js::bridge::SqlxAccessor::arc(&dsn).await.unwrap();
        let rows = db
            .query_with_params(
                "select name from sqlite_master where type='table' and name='oj_svc_token'",
                &[],
            )
            .await
            .unwrap();
        !rows.is_empty()
    }
}

const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;

/// mcp_desc（P1）：声明 description 的路由透传声明值（无契约通道叠加 inspect
/// 提示）；未声明路由回落合成 summary（同一 openapi 生成器——两线字节同源）。
/// auth 在册（本二进制统一引导），allow_anonymous 放行匿名 tools/list。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_description_passthrough_and_summary_fallback() {
    let _g = lock();
    let (addr, _h, _t) = boot_svc(|c| {
        // 匿名放行走 auth.anonymous_paths（mcp.allow_anonymous 只是拒启门禁，
        // 不是鉴权豁免）——守卫按无 base 路径匹配。
        c.auth
            .as_mut()
            .unwrap()
            .anonymous_paths
            .push(only_js::config::AnonPath::Plain("/mcp".into()));
    })
    .await;
    let (_s, v) = rpc_req_auth(addr, "/v1/api/mcp", TOOLS_LIST, "").await;
    let tools = v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list 响应形状异常：{v}"));
    let get_tool = tools
        .iter()
        .find(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.ends_with("user_account_get"))
        })
        .unwrap_or_else(|| panic!("缺 user_account_get：{v}"));
    assert_eq!(
        get_tool["description"],
        "查询账户列表（id 过滤可选） (no declared input contract — inspect source)",
        "声明的 description 透传；无契约通道 → 叠加 inspect 提示（plan 既定叠加在选取值之后）"
    );
    let post_tool = tools
        .iter()
        .find(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.ends_with("user_account_post"))
        })
        .unwrap_or_else(|| panic!("缺 user_account_post：{v}"));
    assert_eq!(
        post_tool["description"],
        "POST v1/api/user/account (no declared input contract — inspect source)",
        "未声明回落合成 summary + inspect 提示（P1 前的形状叠加后缀，字节回归）"
    );
}

/// mcp_scopes（P3，三面矩阵）：read 档 svc token——tools/list 只见 read 档、
/// tools/call 写 tool 得 isError [code=403]、直连 REST DELETE 同 msg、/rpc error
/// 同语义（三面同规则，admit 单点）；无 scopes 旧 token 三面全量（零破坏）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_scopes_three_tiers_three_faces() {
    let _g = lock();
    let (addr, _h, t) = boot_svc(|_| {}).await;
    assert!(
        has_svc_token_table(&t).await,
        "admin 0002 迁移须随 dev boot 跑齐"
    );
    db_exec(
        &t.join("db.sqlite"),
        "INSERT INTO oj_svc_token (jti, name, scopes, created_by, created_at) \
         VALUES ('jt-e2e', 'e2e', '[\"read\"]', 'e2e', 0)",
    )
    .await;
    let read = svc_token(&["read"], json!({}));

    // 面 1（展示裁剪）：tools/list 无写档 tool，read 档 GET tool 在列。
    let (s, v) = rpc_req_auth(addr, "/v1/api/mcp", TOOLS_LIST, &read).await;
    assert_eq!(s, 200, "{v}");
    let tools = v["result"]["tools"].as_array().unwrap();
    let writers = ["_post", "_put", "_patch", "_del", "_delete"];
    for tt in tools {
        let n = tt["name"].as_str().unwrap_or("");
        assert!(
            !writers.iter().any(|w| n.ends_with(w)),
            "read 档不得见写面 tool：{n}"
        );
    }
    assert!(
        tools.iter().any(|t| t["name"]
            .as_str()
            .is_some_and(|n| n.ends_with("user_account_get"))),
        "read 档 GET tool 须在列：{v}"
    );

    // 面 2（tools/call 执行拦）：写 tool 仍可解析（展示裁剪不进调用面）→ admit 拒。
    let (s, v) = rpc_req_auth(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"user_account_post","arguments":{"body":{"name":"x","role":"user"}}}}"#,
        &read,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert_eq!(
        v["result"]["content"][0]["text"], "[code=403] missing scope: write",
        "{v}"
    );

    // 面 3（直连 REST）：DELETE destructive 档拒，信封 msg 同文案。
    let (s, v) = req_auth(addr, "DELETE", "/v1/api/user/account/?id=1", None, &read).await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["msg"], "missing scope: destructive", "{v}");

    // 面 4（/rpc）：同 operationId → JSON-RPC error，code=403 message 带前缀。
    let (s, v) = rpc_req_auth(
        addr,
        "/v1/api/rpc",
        r#"{"jsonrpc":"2.0","id":3,"method":"user_account_post","params":{"body":{"name":"x","role":"user"}}}"#,
        &read,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["error"]["code"], 403, "{v}");
    assert_eq!(
        v["error"]["message"], "[code=403] missing scope: write",
        "{v}"
    );

    // 零破坏：无 scopes 旧 token 三面全量（POST 直达 handler、DELETE 200）。
    let now = serve::test_support::now_secs();
    let legacy = sign_hs256(
        SVC_SECRET,
        json!({ "sub": "9", "roles": ["user"], "iat": now, "exp": now + 3600 }),
    );
    let (_s, v) = rpc_req_auth(addr, "/v1/api/mcp", TOOLS_LIST, &legacy).await;
    let tools = v["result"]["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t["name"]
            .as_str()
            .is_some_and(|n| n.ends_with("user_account_post"))),
        "旧 token 全量可见：{v}"
    );
    let (_s, v) = rpc_req_auth(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"user_account_post","arguments":{"body":{"name":"legacy","role":"user"}}}}"#,
        &legacy,
    )
    .await;
    assert!(
        v["result"].get("isError").is_none(),
        "旧 token 直达 handler：{v}"
    );
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/user/account/",
        Some(r#"{"name":"legacy","role":"user"}"#),
        &legacy,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, _v) = req_auth(addr, "DELETE", "/v1/api/user/account/?id=1", None, &legacy).await;
    assert_eq!(s, 200, "旧 token DELETE 不设防");
}

/// mcp_obo（P2+P3 全矩阵）：admin 签发 svc token + 绑定 → 换签 600s 带 act →
/// 员工权限命中；四失败路径 40310-40313 与 Task 6 契约逐字一致（信封 code 是
/// 契约，>599 落 HTTP 500）；jti 删行后 30s TTL 上限内 /mcp 转 403（deadline
/// 轮询，禁固定 sleep 当同步）；审计落行（分页 data.list）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_obo_full_matrix() {
    let _g = lock();
    let (addr, _h, t) = boot_svc(|_| {}).await;
    let admin = admin_token(json!({}));

    // 签发 + 绑定（wecom/zhang → trinity）。tok2 必须在**首次** /mcp 调用前签发：
    // SvcGate 30s TTL 缓存在首次 admit 时定型，其后新建的 jti 要等缓存过期才可见
    // （创建滞后与吊销滞后同为 30s 上限，设计内）。
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/admin/service-token/",
        Some(r#"{"name":"e2ebot","scopes":["read","write"]}"#),
        &admin,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let svc_tok = v["data"]["token"].as_str().unwrap().to_string();
    let jti = v["data"]["jti"].as_str().unwrap().to_string();
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/admin/service-token/",
        Some(r#"{"name":"e2e2","scopes":["read"]}"#),
        &admin,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tok2 = v["data"]["token"].as_str().unwrap().to_string();
    let jti2 = v["data"]["jti"].as_str().unwrap().to_string();
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/admin/bot-binding/",
        Some(r#"{"provider":"wecom","external_sub":"zhang","user_id":2}"#),
        &admin,
    )
    .await;
    assert_eq!(s, 200, "{v}");

    // 为「roles 缺 bot」纵深腿预播 jt-e2e（须在首次 svc admit 之前落表——SvcGate
    // 30s TTL 快照在首个 jti 门查询时定型，晚了会被 fail-closed 拒成 403）。
    db_exec(
        &t.join("db.sqlite"),
        "INSERT INTO oj_svc_token (jti, name, scopes, created_by, created_at) \
         VALUES ('jt-e2e', 'e2e', '[\"read\"]', 'e2e', 0)",
    )
    .await;

    // 换签：600s + act.sub 指回 svc；无 tenant（部署关时缺失即正形状）。
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"zhang"}"#),
        &svc_tok,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["expires_in"], 600, "{v}");
    let obo_tok = v["data"]["access_token"].as_str().unwrap().to_string();
    let kd = jsonwebtoken::decode::<Value>(
        &obo_tok,
        &jsonwebtoken::DecodingKey::from_secret(SVC_SECRET.as_bytes()),
        &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256),
    )
    .unwrap();
    // 线上 token 是 flatten 形状（sign 侧 claims 平铺；守卫 verify 再归拢 claims）。
    assert_eq!(kd.claims["act"]["sub"], "svc:e2ebot", "{}", kd.claims);
    assert!(
        kd.claims.get("tenant").is_none(),
        "tenant 关部署：换签 token 不带 tenant claim"
    );
    // 员工权限命中：以受托 token 走 GET（roles user）。
    let (s, v) = req_auth(addr, "GET", "/v1/api/user/account/", None, &obo_tok).await;
    assert_eq!(s, 200, "{v}");

    // 四失败路径（Task 6 契约码）。
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"nobody"}"#),
        &svc_tok,
    )
    .await;
    assert_eq!((s, v["code"].as_i64()), (500, Some(40310)), "未绑定：{v}");
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"zhang"}"#),
        &admin,
    )
    .await;
    assert_eq!((s, v["code"].as_i64()), (500, Some(40312)), "非 svc：{v}");
    // 40312 纵深腿：claims.svc/jti 俱在但 roles 缺 bot——同判非 svc（spec P2 双查）。
    // jt-e2e 已在首 admit 前播种（缓存含它），admit 放行到 handler，由 roles 双查收口。
    let now = serve::test_support::now_secs();
    let nobot = sign_hs256(
        SVC_SECRET,
        json!({ "sub": "svc:e2e", "roles": ["user"], "iat": now, "exp": now + 3600,
                "svc": "e2e", "jti": "jt-e2e" }),
    );
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"zhang"}"#),
        &nobot,
    )
    .await;
    assert_eq!(
        (s, v["code"].as_i64()),
        (500, Some(40312)),
        "roles 缺 bot：{v}"
    );
    db_exec(
        &t.join("db.sqlite"),
        "UPDATE users SET disabled = 1 WHERE id = 2",
    )
    .await;
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"zhang"}"#),
        &svc_tok,
    )
    .await;
    assert_eq!((s, v["code"].as_i64()), (500, Some(40313)), "停用：{v}");
    let (s, v) = req_auth(
        addr,
        "DELETE",
        &format!("/v1/api/admin/service-token/{jti}"),
        None,
        &admin,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"zhang"}"#),
        &svc_tok,
    )
    .await;
    assert_eq!((s, v["code"].as_i64()), (500, Some(40311)), "吊销：{v}");

    // 审计：每次 obo 调用（成败均）落一行，经 admin 端点可查（分页 data.list）。
    let (_s, v) = req_auth(addr, "GET", "/v1/api/admin/obo-audit/", None, &admin).await;
    let rows = v["data"]["list"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"));
    assert!(
        rows.iter().any(|r| r["svc"] == "e2ebot" && r["ok"] == 1),
        "成功腿审计行：{v}"
    );
    assert!(
        rows.iter()
            .any(|r| r["ok"] == 0 && r["reason"] == "revoked"),
        "失败腿审计行：{v}"
    );

    // jti 吊销 30s TTL 上限：tok2 直打 /mcp（首缓存已含其 jti）→ 删行后立即仍
    // 200（缓存窗口），deadline 轮询至 403（≤30s 缓存 + 余量）。
    let (s, _v) = rpc_req_auth(addr, "/v1/api/mcp", TOOLS_LIST, &tok2).await;
    assert_eq!(s, 200, "在册 jti 直打 /mcp 须过 admit");
    let (s, _v) = req_auth(
        addr,
        "DELETE",
        &format!("/v1/api/admin/service-token/{jti2}"),
        None,
        &admin,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, _v) = rpc_req_auth(addr, "/v1/api/mcp", TOOLS_LIST, &tok2).await;
    assert_eq!(s, 200, "TTL 缓存窗口内不抖动");
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(45);
    loop {
        let (s, _v) = rpc_req_auth(addr, "/v1/api/mcp", TOOLS_LIST, &tok2).await;
        if s == 403 {
            break;
        }
        assert!(
            std::time::Instant::now() < dl,
            "jti 吊销须在 30s TTL 后全网生效"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}

/// mcp_obo 租户腿（PR-9）：tenant.enable + require_signed_claim 部署下 svc token
/// 带 claims.tenant → 换签 token 原样复制 → 业务面**不带租户头** 200（租户只认
/// 验签后 claim，裸头不自证）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_obo_tenant_copy() {
    let _g = lock();
    let (addr, _h, t) = boot_svc(|c| {
        c.tenant = only_js::config::TenantCfg {
            enable: true,
            require_signed_claim: true,
            header_key: "X-TENANT-ID".into(),
            ..Default::default()
        };
    })
    .await;
    assert!(
        has_svc_token_table(&t).await,
        "admin 0002 迁移须随 dev boot 跑齐"
    );
    db_exec(
        &t.join("db.sqlite"),
        "INSERT INTO oj_svc_token (jti, name, scopes, created_by, created_at) \
         VALUES ('jt-e2e', 'e2e', '[\"read\"]', 'e2e', 0)",
    )
    .await;
    // admin/svc token 都带 claims.tenant=1（租户门只认验签后 claim；管理端点也要过租户门）。
    let admin = admin_token(json!({ "tenant": "1" }));
    let svc_tok = svc_token(&["read"], json!({ "tenant": "1" }));
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/admin/bot-binding/",
        Some(r#"{"provider":"wecom","external_sub":"zhang","user_id":2}"#),
        &admin,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = req_auth(
        addr,
        "POST",
        "/v1/api/auth/obo",
        Some(r#"{"provider":"wecom","external_sub":"zhang"}"#),
        &svc_tok,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let obo_tok = v["data"]["access_token"].as_str().unwrap().to_string();
    let kd = jsonwebtoken::decode::<Value>(
        &obo_tok,
        &jsonwebtoken::DecodingKey::from_secret(SVC_SECRET.as_bytes()),
        &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256),
    )
    .unwrap();
    assert_eq!(
        kd.claims["tenant"], "1",
        "tenant 原样复制（flatten 线上形状）：{}",
        kd.claims
    );
    // 业务面不带 X-TENANT-ID 头——claims.tenant 单独满足租户门。
    let (s, v) = req_auth(addr, "GET", "/v1/api/user/account/", None, &obo_tok).await;
    assert_eq!(s, 200, "{v}");
}
