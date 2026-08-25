//! oj-es：es 轴首个 cdylib 插件（spec §3 es 试点，plan Task 3.4）。
//! 原 core `EsClient` 的 HTTP 实现迁入本插件（url_for/es_resp_b 随之迁移；
//! valid_ident 校验留在 core op 层，插件信任宿主已校验的 index/id）。
//! 插件自建 tokio runtime（spike S.2 定稿形态）；vtable 三方法同步签名返回
//! FfiFuture，宿主 await 其完成。
//!
//! cfg JSON：`{"endpoint": "http://127.0.0.1:9200"}`（单 es 后端，装配注入）。
//! 句柄约定：init 时为 cfg 声明的 endpoint 分配 handle 0——宿主装配 es 轴固定用
//! handle 0（键选式单后端；未来多客户端经 cfg 加 endpoint 条目、handle 顺序分配）。

use oj_plugin_ffi::{
    ABI_VERSION, EsBackendVtable, FfiFuture, HostContext, PluginDescriptor, PluginRegistrations,
    RArc, RBytes, RResult, RString,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

/// 迁入的 es HTTP 客户端（原 core `EsClient`，唯一改动：不再实现 core trait）。
/// Clone 以便锁内拷贝出句柄后于锁外使用。
#[derive(Clone)]
struct EsClientInner {
    endpoint: String,
    http: reqwest::Client,
}

impl EsClientInner {
    fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            http: reqwest::Client::builder().no_proxy().build().unwrap_or_default(),
        }
    }
}

/// 插件共享状态（进程级单例，init 建立）。
struct EsPluginState {
    rt: tokio::runtime::Runtime,
    /// handle → 客户端。init 时为 cfg endpoint 建 handle 0（es 轴无 connect 方法，
    /// 句柄不随调用分配；未来多客户端走 cfg 加 endpoint 条目再行分配）。
    clients: Mutex<HashMap<u64, EsClientInner>>,
}

static PLUGIN: OnceLock<EsPluginState> = OnceLock::new();

fn state() -> &'static EsPluginState {
    PLUGIN.get().expect("oj-es: init not called")
}

// ---- FfiFuture 桥（S.2 定稿：oneshot 接结果，poll 消费式暂存）----

/// 共享状态：插件 runtime 上的任务经 oneshot 回传结果；poll 收到后暂存（try_recv 消费式）。
struct EsCallState {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut EsCallState) };
    if let Some(r) = &s.result {
        return if r.is_ok() { 1 } else { -1 };
    }
    match s.rx.try_recv() {
        Ok(r) => {
            let code = if r.is_ok() { 1 } else { -1 };
            s.result = Some(r);
            code
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => 0,
        // sender 被 drop 却没 send（任务 panic）→ 视为 error
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => -1,
    }
}

extern "C" fn take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &mut *(state as *mut EsCallState) };
    match s.result.take() {
        Some(Ok(bytes)) => {
            let mut v = RBytes::new();
            for b in bytes {
                v.push(b);
            }
            RResult::Ok(v)
        }
        Some(Err(e)) => RResult::Err(RString::from(e.as_str())),
        None => RResult::Err(RString::from("take before ready or twice")),
    }
}

extern "C" fn free(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut EsCallState) });
    }
}

/// 起一个 FfiFuture：把异步工作 spawn 到插件 runtime，oneshot 收结果。
fn spawn_call(fut: impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state().rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    FfiFuture {
        state: Box::into_raw(Box::new(EsCallState { rx, result: None })).cast(),
        poll,
        take,
        free,
    }
}

// ---- 迁移自 core es.rs 的 HTTP 实现 ----

/// 响应直通：2xx → JSON 值；非 2xx → Err（带 ES 返回体便于排障）。
async fn es_resp_b(resp: reqwest::Response, what: &str) -> Result<serde_json::Value, String> {
    let status = resp.status();
    if status.is_success() {
        resp.json().await.map_err(|e| format!("{what}: parse response: {e}"))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("{what}: HTTP {status}: {body}"))
    }
}

/// 路径拼装（endpoint 尾斜杠幂等剪除）：None → `/{index}/_search`；
/// Some → `/{index}/_doc/{id}?refresh=true`。
fn url_for(endpoint: &str, index: &str, id: Option<&str>) -> String {
    let base = endpoint.trim_end_matches('/');
    match id {
        Some(id) => format!("{base}/{index}/_doc/{id}?refresh=true"),
        None => format!("{base}/{index}/_search"),
    }
}

impl EsClientInner {
    async fn search(&self, index: &str, body: &str) -> Result<Vec<u8>, String> {
        let dsl: serde_json::Value =
            serde_json::from_str(body).map_err(|e| format!("es search: parse body: {e}"))?;
        let url = url_for(&self.endpoint, index, None);
        let resp = self
            .http
            .post(&url)
            .json(&dsl)
            .send()
            .await
            .map_err(|e| format!("es search: {e}"))?;
        let v = es_resp_b(resp, "es search").await?;
        serde_json::to_vec(&v).map_err(|e| format!("es search: serialize: {e}"))
    }

    async fn index_doc(&self, index: &str, id: &str, doc: &str) -> Result<Vec<u8>, String> {
        let doc: serde_json::Value =
            serde_json::from_str(doc).map_err(|e| format!("es index: parse body: {e}"))?;
        let url = url_for(&self.endpoint, index, Some(id));
        let resp = self
            .http
            .put(&url)
            .json(&doc)
            .send()
            .await
            .map_err(|e| format!("es index: {e}"))?;
        let v = es_resp_b(resp, "es index").await?;
        serde_json::to_vec(&v).map_err(|e| format!("es index: serialize: {e}"))
    }

    async fn delete_doc(&self, index: &str, id: &str) -> Result<Vec<u8>, String> {
        let url = url_for(&self.endpoint, index, Some(id));
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .map_err(|e| format!("es del: {e}"))?;
        let v = es_resp_b(resp, "es del").await?;
        serde_json::to_vec(&v).map_err(|e| format!("es del: serialize: {e}"))
    }
}

impl EsPluginState {
    /// handle → 客户端（锁内拷贝，锁外使用；handle 未知 = 装配期外调用，报错）。
    fn client(&self, handle: u64) -> Result<EsClientInner, String> {
        self.clients
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("es: unknown handle {handle}"))
    }

    async fn do_search(&self, handle: u64, index: &str, body: &str) -> Result<Vec<u8>, String> {
        self.client(handle)?.search(index, body).await
    }

    async fn do_index(
        &self,
        handle: u64,
        index: &str,
        id: &str,
        doc: &str,
    ) -> Result<Vec<u8>, String> {
        self.client(handle)?.index_doc(index, id, doc).await
    }

    async fn do_delete(&self, handle: u64, index: &str, id: &str) -> Result<Vec<u8>, String> {
        self.client(handle)?.delete_doc(index, id).await
    }
}

// ---- vtable 三方法 + close（同步签名返回 FfiFuture）----

extern "C" fn search(handle: u64, index: RString, body: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_search(handle, &index[..], &body[..]).await })
}

extern "C" fn index_doc(handle: u64, index: RString, id: RString, body: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_index(handle, &index[..], &id[..], &body[..]).await })
}

extern "C" fn delete_doc(handle: u64, index: RString, id: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_delete(handle, &index[..], &id[..]).await })
}

extern "C" fn close(handle: u64) {
    state().clients.lock().unwrap().remove(&handle);
}

static ES_VTABLE: EsBackendVtable = EsBackendVtable { search, index_doc, delete_doc, close };

extern "C" fn register() -> PluginRegistrations {
    PluginRegistrations { es: &ES_VTABLE, db: std::ptr::null(), blob: std::ptr::null() }
}

// ---- 入口 ----

/// 插件 descriptor（常量字段，重入/复用路径共用）。
fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("es"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        register,
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 同进程二次 init（多装配/测试重载同一 dylib）：cfg 以首次为准，直接复用 descriptor。
    // 生产每进程每插件一次装配，此分支不触发；状态（PLUGIN/runtime/客户端）只建一次。
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }

    let cfg_v: serde_json::Value = serde_json::from_str(&cfg[..]).unwrap_or(serde_json::json!({}));
    let endpoint = cfg_v.get("endpoint").and_then(|v| v.as_str()).map(str::to_string);

    let mut clients = HashMap::new();
    if let Some(ep) = endpoint.as_deref() {
        if !ep.is_empty() {
            clients.insert(0, EsClientInner::new(ep.to_string()));
            (host.log)(2, RString::from(format!("oj-es: es backend ready, endpoint {ep}")));
        }
    }
    let st = EsPluginState { rt: runtime(), clients: Mutex::new(clients) };
    // 并发重入：另一线程已建好 → 复用。
    let _ = PLUGIN.set(st);

    RResult::Ok(descriptor())
}

/// 插件自建 tokio runtime（跨 FFI 不共享宿主 tokio，规避 TLS 双副本，S.2 定稿）。
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-es tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init);

#[cfg(test)]
mod tests {
    use super::*;
    use httptest::{Expectation, Server, matchers::*, responders::*};
    use serde_json::Value;

    extern "C" fn test_log(_level: u8, _msg: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext { log: test_log })
    }

    /// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询；插件任务跑在插件
    /// runtime，测试 runtime 的 yield_now 让出即可被并发调度）。
    async fn drive(fut: FfiFuture) -> Result<Vec<u8>, String> {
        let mut fut = fut;
        for _ in 0..100_000 {
            match (fut.poll)(fut.state) {
                0 => tokio::task::yield_now().await,
                code => {
                    let r = (fut.take)(fut.state);
                    (fut.free)(fut.state);
                    // FfiFuture 在本 crate 无 Drop 实现（free-on-drop 是宿主 FfiGuard 的
                    // 职责），take+free 后直接返回，无二次释放。
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Err(e)) => Err(e[..].to_string()),
                        _ => Err("ffi poll reported error but take succeeded".into()),
                    };
                }
            }
        }
        Err("ffi drive timeout".into())
    }

    #[test]
    fn url_for_trims_trailing_slash() {
        assert_eq!(url_for("http://localhost:9200", "user", None), "http://localhost:9200/user/_search");
        assert_eq!(url_for("http://localhost:9200/", "user", None), "http://localhost:9200/user/_search");
        assert_eq!(
            url_for("http://localhost:9200", "user", Some("d1")),
            "http://localhost:9200/user/_doc/d1?refresh=true"
        );
    }

    /// 迁移自 core 的 HTTP 行为测试：直接构造 EsClientInner（不触碰 PLUGIN 单例，
    /// 各测试可用独立 mock server，无 init 顺序竞争）。
    #[tokio::test(flavor = "current_thread")]
    async fn search_index_del_hit_mock_server() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(200).body(r#"{"hits":{"total":{"value":3}}}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("PUT"),
                request::path("/user/_doc/d1")
            ])
            .respond_with(status_code(200).body(r#"{"result":"created"}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("DELETE"),
                request::path("/user/_doc/d1")
            ])
            .respond_with(status_code(200).body(r#"{"result":"deleted"}"#)),
        );

        let c = EsClientInner::new(server.url("/").to_string());
        let v: Value = serde_json::from_slice(&c.search("user", "{}").await.unwrap()).unwrap();
        assert_eq!(v["hits"]["total"]["value"], 3, "{v}");
        let v: Value = serde_json::from_slice(&c.index_doc("user", "d1", r#"{"a":1}"#).await.unwrap()).unwrap();
        assert_eq!(v["result"], "created", "{v}");
        let v: Value = serde_json::from_slice(&c.delete_doc("user", "d1").await.unwrap()).unwrap();
        assert_eq!(v["result"], "deleted", "{v}");
    }

    /// 非 2xx → 错误带状态码与响应体；2xx 但非 JSON → parse 错误。
    #[tokio::test(flavor = "current_thread")]
    async fn non_2xx_and_bad_json_propagate_errors() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(500).body("es exploded")),
        );
        let c = EsClientInner::new(server.url("/").to_string());
        let e = c.search("user", "{}").await.unwrap_err();
        assert!(e.contains("HTTP 500") && e.contains("es exploded"), "{e}");

        let server2 = Server::run();
        server2.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(200).body("not-json")),
        );
        let c2 = EsClientInner::new(server2.url("/").to_string());
        let e = c2.search("user", "{}").await.unwrap_err();
        assert!(e.contains("parse response"), "{e}");
    }

    /// PLUGIN 单例只能 init 一次：本测试经 init 建立（指向 mock server），串行驱动
    /// vtable 三方法经插件自建 runtime 执行（spec §3 异步跨边界 + S.2 形态实证）。
    /// 与其它测试无 PLUGIN 竞争（其余测试不触碰单例）。
    #[tokio::test(flavor = "multi_thread")]
    async fn vtable_three_methods_roundtrip_via_plugin_runtime() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(200).body(r#"{"hits":{"total":{"value":3}}}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("PUT"),
                request::path("/user/_doc/d1")
            ])
            .respond_with(status_code(200).body(r#"{"result":"created"}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("DELETE"),
                request::path("/user/_doc/d1")
            ])
            .respond_with(status_code(200).body(r#"{"result":"deleted"}"#)),
        );

        let cfg = serde_json::json!({ "endpoint": server.url("/").to_string() }).to_string();
        let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
            Ok(d) => d,
            Err(e) => panic!("oj-es init failed: {}", e[..].to_string()),
        };
        assert_eq!(&desc.name[..], "es");
        assert_eq!(desc.abi_version, ABI_VERSION);

        let bytes = drive(search(0, RString::from("user"), RString::from("{}")))
            .await
            .expect("search");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["hits"]["total"]["value"], 3, "{v}");

        let bytes = drive(index_doc(0, RString::from("user"), RString::from("d1"), RString::from(r#"{"a":1}"#)))
            .await
            .expect("index_doc");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["result"], "created", "{v}");

        let bytes = drive(delete_doc(0, RString::from("user"), RString::from("d1")))
            .await
            .expect("delete_doc");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["result"], "deleted", "{v}");

        // 未知 handle → 错误（装配期外调用兜底）。
        let e = drive(search(999, RString::from("user"), RString::from("{}"))).await.unwrap_err();
        assert!(e.contains("unknown handle 999"), "{e}");
    }
}
