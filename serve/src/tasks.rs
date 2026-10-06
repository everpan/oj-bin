//! 任务管理 API（PRD v2 §6.6 最小集）：`{base}/tasks` 任务控制面。
//! 数据源 = only_js TaskRegistry（内存状态机权威）+ TaskEventLog（执行历史）。
//! 鉴权/租户与业务路由同一语义（AuthGuard.verify + tenant_header 管线对齐 run_route）；
//! 本层不接受任何用户提供的脚本路径（PATCH 仅 enabled/cron 表达式），scriptPath
//! 白名单约束由「无路径入口」结构性满足（FR-API-SEC-001）。
//! ponytail: 任务为实例级资源（source of truth = 文件系统），本期不做按租户隔离
//! 的任务视图——租户头强制与鉴权照常生效，per-tenant 任务命名空间入 backlog。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use only_js::bridge::AuthGuard;
use only_js::bridge::task_pool::{
    CronExpr, TaskEntry, TaskKind, TaskPool, TaskRegistry, TaskStatus,
};

/// 任务 API 状态：注册表 + 池句柄（run-once 派发）+ 守卫/租户配置。
#[derive(Clone)]
pub struct TasksApiState {
    pub registry: Arc<TaskRegistry>,
    pub pool: Option<Arc<TaskPool>>,
    pub auth: Option<Arc<dyn AuthGuard>>,
    pub tenant_header: Option<String>,
    pub tenant_anon: Vec<String>,
}

fn json_response(body: Vec<u8>) -> Response {
    let mut r = Response::new(axum::body::Body::from(body));
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    r
}

fn fail_response(status: u16, msg: &str) -> Response {
    let mut r =
        json_response(only_js::bridge::fail(status as i32, msg, &serde_json::Value::Null).0);
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r
}

fn ts(t: Option<std::time::SystemTime>) -> serde_json::Value {
    t.map(|t| serde_json::Value::String(chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()))
        .unwrap_or(serde_json::Value::Null)
}

fn entry_json(e: &TaskEntry) -> serde_json::Value {
    let (kind, next_run) = match &e.kind {
        TaskKind::Long => ("long", None),
        TaskKind::Cron { next_run, .. } => ("cron", *next_run),
    };
    serde_json::json!({
        "name": e.name,
        "type": kind,
        "path": e.path.to_string_lossy(),
        "enabled": e.enabled,
        "status": format!("{:?}", e.status).to_lowercase(),
        "runCount": e.run_count,
        "lastError": e.last_error,
        "startedAt": ts(e.started_at),
        "finishedAt": ts(e.finished_at),
        "nextRun": ts(next_run),
    })
}

/// 守卫 + 租户管线（与 run_route 同语义；path_no_base = base 之后的部分，
/// 匿名路径判定复用 serve::path_matches）。
fn admitted(
    st: &TasksApiState,
    method: &str,
    path_no_base: &str,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    if let Some(g) = &st.auth {
        let header = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        // ABI 9：方法 + 全部请求头 JSON 透传守卫（同 serve::run_route 语义）。
        let headers_json = serde_json::to_string(
            &headers
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str(), s)))
                .collect::<std::collections::HashMap<_, _>>(),
        )
        .unwrap_or_default();
        if let Err(msg) = g.verify(path_no_base, method, header, Some(&headers_json)) {
            return Err(Box::new(fail_response(401, &msg)));
        }
    }
    if let Some(key) = &st.tenant_header {
        let exempt = crate::path_matches(&st.tenant_anon, path_no_base);
        let present = headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| !s.is_empty());
        if !present && !exempt {
            return Err(Box::new(fail_response(
                400,
                &format!("missing tenant header: {key}"),
            )));
        }
    }
    Ok(())
}

/// 挂载 `{base}/tasks` 管理端点（在 fallback 之前 merge 进主 router）。
pub fn tasks_router(base: &str, state: TasksApiState) -> axum::Router {
    let base = base.trim_end_matches('/');
    let p = format!("{base}/tasks");
    let one = format!("{p}/{{name}}");
    let logs = format!("{p}/{{name}}/logs");
    let start = format!("{p}/{{name}}/start");
    let stop = format!("{p}/{{name}}/stop");
    let reload = format!("{p}/{{name}}/reload");
    let run_once = format!("{p}/{{name}}/run-once");
    let enable = format!("{p}/{{name}}/enable");
    let disable = format!("{p}/{{name}}/disable");
    axum::Router::new()
        .route(&p, axum::routing::get(list_handler))
        .route(
            &one,
            axum::routing::get(get_handler)
                .patch(patch_handler)
                .delete(delete_handler),
        )
        .route(&logs, axum::routing::get(logs_handler))
        .route(&start, axum::routing::post(start_handler))
        .route(&stop, axum::routing::post(stop_handler))
        .route(&reload, axum::routing::post(reload_handler))
        .route(&run_once, axum::routing::post(run_once_handler))
        .route(&enable, axum::routing::post(enable_handler))
        .route(&disable, axum::routing::post(disable_handler))
        .with_state(state)
}

async fn list_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = admitted(&st, "GET", "/tasks", &headers) {
        return *r;
    }
    let filter = q.get("type").cloned();
    let items: Vec<_> = st
        .registry
        .list()
        .into_iter()
        .filter(|e| match (&filter, &e.kind) {
            (Some(t), TaskKind::Long) => t == "long",
            (Some(t), TaskKind::Cron { .. }) => t == "cron",
            (None, _) => true,
        })
        .map(|e| entry_json(&e))
        .collect();
    json_response(only_js::bridge::ok(&serde_json::json!({ "items": items })))
}

async fn get_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "GET", "/tasks/*", &headers) {
        return *r;
    }
    match st.registry.get(&name) {
        Some(e) => json_response(only_js::bridge::ok(&entry_json(&e))),
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

async fn logs_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = admitted(&st, "GET", "/tasks/*/logs", &headers) {
        return *r;
    }
    if st.registry.get(&name).is_none() {
        return fail_response(404, &format!("task '{name}' not found"));
    }
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);
    let events: Vec<_> = st
        .registry
        .log
        .recent(limit)
        .into_iter()
        .filter(|v| v["event"]["payload"]["task"].as_str() == Some(name.as_str()))
        .map(|v| v["event"].clone())
        .collect();
    json_response(only_js::bridge::ok(&serde_json::json!({ "items": events })))
}

async fn start_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "POST", "/tasks/*/start", &headers) {
        return *r;
    }
    match st.registry.get(&name) {
        Some(TaskEntry {
            kind: TaskKind::Cron { .. },
            ..
        }) => fail_response(400, "cron tasks use /enable — /start is for long tasks"),
        Some(_) => {
            st.registry.start(&name);
            json_response(only_js::bridge::ok(&serde_json::json!({ "started": name })))
        }
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

async fn stop_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "POST", "/tasks/*/stop", &headers) {
        return *r;
    }
    match st.registry.get(&name) {
        Some(TaskEntry {
            kind: TaskKind::Cron { .. },
            ..
        }) => fail_response(400, "cron tasks use /disable — /stop is for long tasks"),
        Some(_) => {
            st.registry.set_enabled(&name, false);
            json_response(only_js::bridge::ok(&serde_json::json!({ "stopped": name })))
        }
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

async fn reload_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "POST", "/tasks/*/reload", &headers) {
        return *r;
    }
    match st.registry.get(&name) {
        Some(TaskEntry {
            kind: TaskKind::Cron { .. },
            ..
        }) => fail_response(400, "reload is only supported for long tasks"),
        Some(_) => {
            st.registry.request_reload(&name);
            json_response(only_js::bridge::ok(&serde_json::json!({ "reload": name })))
        }
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

async fn enable_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "POST", "/tasks/*/enable", &headers) {
        return *r;
    }
    match st.registry.get(&name) {
        Some(TaskEntry {
            kind: TaskKind::Long,
            ..
        }) => fail_response(400, "long tasks use /start — /enable is for cron tasks"),
        Some(_) => {
            st.registry.set_enabled(&name, true);
            json_response(only_js::bridge::ok(&serde_json::json!({ "enabled": name })))
        }
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

async fn disable_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "POST", "/tasks/*/disable", &headers) {
        return *r;
    }
    match st.registry.get(&name) {
        Some(TaskEntry {
            kind: TaskKind::Long,
            ..
        }) => fail_response(400, "long tasks use /stop — /disable is for cron tasks"),
        Some(_) => {
            st.registry.set_enabled(&name, false);
            json_response(only_js::bridge::ok(
                &serde_json::json!({ "disabled": name }),
            ))
        }
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

async fn run_once_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "POST", "/tasks/*/run-once", &headers) {
        return *r;
    }
    let entry = match st.registry.get(&name) {
        Some(
            e @ TaskEntry {
                kind: TaskKind::Cron { .. },
                ..
            },
        ) => e,
        Some(_) => return fail_response(400, "run-once is only supported for cron tasks"),
        None => return fail_response(404, &format!("task '{name}' not found")),
    };
    if entry.status == TaskStatus::Running {
        return fail_response(409, &format!("task '{name}' is already running"));
    }
    let Some(pool) = &st.pool else {
        return fail_response(503, "task pool not available");
    };
    pool.submit_once(only_js::bridge::task_pool::OnceJob {
        name: entry.name.clone(),
        path: entry.path.clone(),
    });
    st.registry.log.record(
        "tasks.commands",
        "cron.run_once",
        serde_json::json!({ "task": entry.name }),
    );
    json_response(only_js::bridge::ok(
        &serde_json::json!({ "runOnce": entry.name }),
    ))
}

/// PATCH：仅接受 { enabled?: bool, cron?: "<5 段表达式>" }，cron 任务专用字段；
/// 无任何路径字段入口（FR-API-SEC-001）。
async fn patch_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: String,
) -> Response {
    if let Err(r) = admitted(&st, "GET", "/tasks/*", &headers) {
        return *r;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return fail_response(400, "invalid JSON body");
    };
    let entry = match st.registry.get(&name) {
        Some(e) => e,
        None => return fail_response(404, &format!("task '{name}' not found")),
    };
    if let Some(cron) = v.get("cron").and_then(|c| c.as_str()) {
        let TaskKind::Cron { .. } = &entry.kind else {
            return fail_response(400, "'cron' field is only valid for cron tasks");
        };
        let expr = match CronExpr::parse(cron) {
            Ok(e) => e,
            Err(e) => return fail_response(400, &e),
        };
        st.registry.set_cron_expr(&name, expr);
    }
    if let Some(en) = v.get("enabled").and_then(|e| e.as_bool()) {
        st.registry.set_enabled(&name, en);
    }
    match st.registry.get(&name) {
        Some(e) => json_response(only_js::bridge::ok(&entry_json(&e))),
        None => fail_response(404, &format!("task '{name}' not found")),
    }
}

/// DELETE：运行期注销（文件系统为 source of truth——重启重扫会重新注册；
/// 删除任务文件才会永久移除）。
async fn delete_handler(
    State(st): State<TasksApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admitted(&st, "GET", "/tasks/*", &headers) {
        return *r;
    }
    if st.registry.remove(&name).is_none() {
        return fail_response(404, &format!("task '{name}' not found"));
    }
    st.registry.log.record(
        "tasks.commands",
        "task.deleted",
        serde_json::json!({ "task": name }),
    );
    json_response(only_js::bridge::ok(&serde_json::json!({ "deleted": name })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::task_pool::{CronExpr, TaskEntry, TaskEventLog, TaskRegistry, TaskStatus};
    use std::path::PathBuf;

    struct RejectGuard;
    impl only_js::bridge::AuthGuard for RejectGuard {
        fn verify(
            &self,
            _path: &str,
            _method: &str,
            _auth: Option<&str>,
            _headers: Option<&str>,
        ) -> Result<Option<serde_json::Value>, String> {
            Err("unauthorized".into())
        }
    }

    fn fixture_registry() -> Arc<TaskRegistry> {
        let reg = TaskRegistry::new(TaskEventLog::memory_only());
        reg.upsert(TaskEntry::long(
            "orders",
            PathBuf::from("/x/tasks/task_orders.ts"),
        ));
        reg.upsert(TaskEntry::cron(
            "nightly",
            PathBuf::from("/x/task/jobs/nightly.ts"),
            CronExpr::parse("30 2 * * *").unwrap(),
        ));
        reg
    }

    fn state(reg: Arc<TaskRegistry>) -> TasksApiState {
        TasksApiState {
            registry: reg,
            pool: None,
            auth: None,
            tenant_header: None,
            tenant_anon: Vec::new(),
        }
    }

    async fn spawn(state: TasksApiState) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = tasks_router("/v1/api", state);
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    async fn raw(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        raw(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"),
        )
        .await
    }

    async fn post(addr: std::net::SocketAddr, path: &str) -> String {
        raw(
            addr,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await
    }

    fn body(resp: &str) -> serde_json::Value {
        let json = resp.split("\r\n\r\n").nth(1).unwrap_or("{}");
        serde_json::from_str(json).unwrap()
    }

    #[tokio::test]
    async fn given_registry_when_list_and_get_then_envelope() {
        let reg = fixture_registry();
        let addr = spawn(state(reg.clone())).await;
        let resp = get(addr, "/v1/api/tasks").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let v = body(&resp);
        assert_eq!(v["code"], 0);
        assert_eq!(v["data"]["items"].as_array().unwrap().len(), 2);

        let resp = get(addr, "/v1/api/tasks?type=cron").await;
        let v = body(&resp);
        assert_eq!(v["data"]["items"][0]["name"], "nightly");
        assert!(v["data"]["items"][0]["nextRun"].is_string());

        let resp = get(addr, "/v1/api/tasks/orders").await;
        let v = body(&resp);
        assert_eq!(v["data"]["type"], "long");
        assert_eq!(v["data"]["status"], "pending");

        let resp = get(addr, "/v1/api/tasks/nope").await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
    }

    #[tokio::test]
    async fn given_commands_when_start_stop_then_registry_follows() {
        let reg = fixture_registry();
        let addr = spawn(state(reg.clone())).await;
        let resp = post(addr, "/v1/api/tasks/orders/start").await;
        assert!(body(&resp)["code"] == 0, "{resp}");
        assert!(reg.get("orders").unwrap().enabled);

        let resp = post(addr, "/v1/api/tasks/orders/stop").await;
        assert!(body(&resp)["code"] == 0, "{resp}");
        assert!(!reg.get("orders").unwrap().enabled);

        // 跨类命令：long 用 /enable → 400 提示。
        let resp = post(addr, "/v1/api/tasks/orders/enable").await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        // cron 用 /stop → 400 提示。
        let resp = post(addr, "/v1/api/tasks/nightly/stop").await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        // cron enable/disable 正常。
        let resp = post(addr, "/v1/api/tasks/nightly/disable").await;
        assert!(body(&resp)["code"] == 0, "{resp}");
        assert!(!reg.get("nightly").unwrap().enabled);

        // run-once 无池 → 503；有任务但非 cron → 400。
        let resp = post(addr, "/v1/api/tasks/orders/run-once").await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        let resp = post(addr, "/v1/api/tasks/nightly/run-once").await;
        assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
    }

    #[tokio::test]
    async fn given_patch_when_bad_cron_then_400_and_good_then_next_run_updated() {
        let reg = fixture_registry();
        let addr = spawn(state(reg.clone())).await;
        let patch = |payload: &'static str| {
            let req = format!(
                "PATCH /v1/api/tasks/nightly HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let req = std::sync::Arc::new(req);
            async move { raw(addr, &req).await }
        };
        let resp = patch("{\"cron\":\"bad expr\"}").await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        let before = reg.get("nightly").unwrap();
        let TaskKind::Cron { next_run, .. } = &before.kind else {
            panic!("nightly must be cron")
        };
        let resp = patch("{\"cron\":\"0 4 * * *\",\"enabled\":true}").await;
        let v = body(&resp);
        assert_eq!(v["code"], 0, "{resp}");
        assert!(reg.get("nightly").unwrap().enabled);
        let TaskKind::Cron {
            next_run: after, ..
        } = &reg.get("nightly").unwrap().kind
        else {
            panic!("nightly must be cron")
        };
        assert_ne!(next_run, after, "next_run must be recomputed");
    }

    #[tokio::test]
    async fn given_delete_when_remove_then_gone_until_404() {
        let reg = fixture_registry();
        let addr = spawn(state(reg.clone())).await;
        let resp = raw(
            addr,
            "DELETE /v1/api/tasks/nightly HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(body(&resp)["code"] == 0, "{resp}");
        assert!(reg.get("nightly").is_none());
        let resp = get(addr, "/v1/api/tasks/nightly").await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
    }

    #[tokio::test]
    async fn given_reject_guard_when_any_endpoint_then_401() {
        let mut st = state(fixture_registry());
        st.auth = Some(Arc::new(RejectGuard));
        let addr = spawn(st).await;
        let resp = get(addr, "/v1/api/tasks").await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        let resp = post(addr, "/v1/api/tasks/orders/start").await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
    }

    #[tokio::test]
    async fn given_tenant_header_when_missing_then_400() {
        let mut st = state(fixture_registry());
        st.tenant_header = Some("X-Tenant".into());
        let addr = spawn(st).await;
        let resp = get(addr, "/v1/api/tasks").await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        // 带头 → 放行。
        let resp = raw(
            addr,
            "GET /v1/api/tasks HTTP/1.1\r\nHost: t\r\nX-Tenant: t1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    }

    #[tokio::test]
    async fn given_logs_endpoint_when_events_then_filtered_by_task() {
        let reg = fixture_registry();
        reg.log.record(
            "tasks.execution",
            "task.started",
            serde_json::json!({ "task": "orders" }),
        );
        reg.log.record(
            "tasks.execution",
            "task.started",
            serde_json::json!({ "task": "nightly" }),
        );
        let addr = spawn(state(reg)).await;
        let resp = get(addr, "/v1/api/tasks/orders/logs").await;
        let v = body(&resp);
        let items = v["data"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{v}");
        assert_eq!(items[0]["eventType"], "task.started");
    }

    #[tokio::test]
    async fn given_failed_task_when_start_then_error_cleared() {
        let reg = fixture_registry();
        reg.set_status("orders", TaskStatus::Failed, Some("boom".into()));
        reg.set_enabled("orders", false);
        let addr = spawn(state(reg.clone())).await;
        let resp = post(addr, "/v1/api/tasks/orders/start").await;
        assert!(body(&resp)["code"] == 0, "{resp}");
        let e = reg.get("orders").unwrap();
        assert_eq!(e.status, TaskStatus::Pending);
        assert_eq!(e.last_error, None);
    }
}
