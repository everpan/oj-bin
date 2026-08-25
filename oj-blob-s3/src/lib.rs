//! oj-blob-s3：blob 轴 s3 cdylib 插件（Task 4.2；S3Blob 自 core blob.rs 迁入）。
//! 迁移决策同 db 插件（spec §3 插件自包含）：valid_key/os_path 逐字复制自 core，
//! 行为与下线前的 S3Blob 对齐（bucket/region 必填 fail-fast、url = GET presign 15min、
//! content_type 恒 None——S3 侧对象自身元数据负责）。
//!
//! cfg 契约：init cfg = `{}`；每后端的 connect(name, cfg) 收 BlobCfg JSON
//! （driver/root 忽略；endpoint/access_key/secret_key 可选，path_style 默认 false）。
//! 句柄约定：connect 分配 handle（AtomicU64），close 释放。

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::signer::Signer;
use object_store::{ObjectStore, PutPayload};
use oj_plugin_ffi::{
    ABI_VERSION, BlobBackendVtable, FfiFuture, HostContext, PluginDescriptor, PluginRegistrations,
    RArc, RBytes, RResult, RString,
};
use reqwest::Method;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 插件侧配置视图（= core config::BlobCfg 的 JSON；serde 只取插件关心的字段）。
#[derive(Deserialize)]
#[serde(default)]
struct S3Cfg {
    driver: String,
    root: String,
    endpoint: Option<String>,
    bucket: Option<String>,
    region: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    path_style: bool,
}

impl Default for S3Cfg {
    fn default() -> Self {
        Self {
            driver: String::new(),
            root: String::new(),
            endpoint: None,
            bucket: None,
            region: None,
            access_key: None,
            secret_key: None,
            path_style: false,
        }
    }
}

/// 插件共享状态（进程级单例，init 建立）。
struct BlobPluginState {
    rt: tokio::runtime::Runtime,
    stores: Mutex<HashMap<u64, Arc<AmazonS3>>>,
    next_handle: AtomicU64,
}

static PLUGIN: OnceLock<BlobPluginState> = OnceLock::new();

fn state() -> &'static BlobPluginState {
    PLUGIN.get().expect("oj-blob-s3: init not called")
}

// ---- FfiFuture 桥（spike S.2 定稿；同 db 插件）----

struct CallState {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut CallState) };
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
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => -1,
    }
}

extern "C" fn take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &mut *(state as *mut CallState) };
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
        drop(unsafe { Box::from_raw(state as *mut CallState) });
    }
}

/// 起一个 FfiFuture：异步工作 spawn 到插件 runtime，oneshot 收结果。
fn spawn_call(fut: impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state().rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    FfiFuture {
        state: Box::into_raw(Box::new(CallState { rx, result: None })).cast(),
        poll,
        take,
        free,
    }
}

// ---- s3 逻辑（迁自 core S3Blob + blob.rs 的 key 校验，语义对齐）----

/// key 白名单：'/' 分段，每段非空、非 `.`/`..`、不含 `\`/`\0`；整串非空、不以 `/` 开头。
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && key.split('/').all(|s| {
            !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', '\0'])
        })
}

fn os_path(key: &str) -> Result<Path, String> {
    valid_key(key).then(|| Path::from(key)).ok_or_else(|| format!("invalid blob key '{key}'"))
}

impl BlobPluginState {
    fn store(&self, handle: u64) -> Result<Arc<AmazonS3>, String> {
        self.stores
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("blob: unknown handle {handle}"))
    }

    async fn do_put(&self, handle: u64, key: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let path = os_path(key)?;
        self.store(handle)?
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .map_err(|e| format!("blob put: {e}"))?;
        Ok(b"".to_vec())
    }

    async fn do_get(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        let path = os_path(key)?;
        let r = self.store(handle)?.get(&path).await.map_err(|e| format!("blob get: {e}"))?;
        Ok(r.bytes().await.map_err(|e| format!("blob get: {e}"))?.to_vec())
    }

    async fn do_del(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        let path = os_path(key)?;
        match self.store(handle)?.delete(&path).await {
            Ok(()) => {}
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(format!("blob del: {e}")),
        }
        Ok(b"".to_vec())
    }

    async fn do_url(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        let path = os_path(key)?;
        let u = self
            .store(handle)?
            .signed_url(Method::GET, &path, std::time::Duration::from_secs(15 * 60))
            .await
            .map_err(|e| format!("blob s3 sign: {e}"))?
            .to_string();
        Ok(u.into_bytes())
    }
}

/// 配置校验 + 建 store（bucket/region 必填 fail-fast；endpoint/access_key/secret_key 可选）。
fn build_store(c: &S3Cfg) -> Result<Arc<AmazonS3>, String> {
    let bucket = c
        .bucket
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "blob s3: bucket required".to_string())?;
    let region = c
        .region
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "blob s3: region required".to_string())?;
    let mut b = AmazonS3Builder::new()
        .with_bucket_name(bucket)
        .with_region(region)
        // 默认 virtual-hosted 风格；path_style（MinIO/自建）切回。
        .with_virtual_hosted_style_request(!c.path_style);
    if let Some(e) = c.endpoint.as_deref().filter(|s| !s.is_empty()) {
        b = b.with_endpoint(e);
    }
    if let Some(k) = c.access_key.as_deref().filter(|s| !s.is_empty()) {
        b = b.with_access_key_id(k);
    }
    if let Some(k) = c.secret_key.as_deref().filter(|s| !s.is_empty()) {
        b = b.with_secret_access_key(k);
    }
    Ok(Arc::new(b.build().map_err(|e| format!("blob s3 build: {e}"))?))
}

// ---- vtable（同步签名返回 FfiFuture；connect 产 handle，close 释放）----

extern "C" fn connect(name: RString, cfg: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move {
        let cfg: S3Cfg = serde_json::from_str(&cfg[..]).map_err(|e| format!("blob s3: bad cfg: {e}"))?;
        let store = build_store(&cfg)?;
        let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
        st.stores.lock().unwrap().insert(handle, store);
        let _ = &name; // 注册名透传（url 裁决保留签名）；s3 presign 对所有名字可用
        Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
    })
}

extern "C" fn put(handle: u64, key: RString, bytes: RBytes, _content_type: RString) -> FfiFuture {
    let st = state();
    let mut b = Vec::with_capacity(bytes.len());
    for x in &bytes {
        b.push(*x);
    }
    spawn_call(async move { st.do_put(handle, &key[..], &b).await })
}

extern "C" fn get(handle: u64, key: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_get(handle, &key[..]).await })
}

extern "C" fn del(handle: u64, key: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_del(handle, &key[..]).await })
}

extern "C" fn url(handle: u64, key: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_url(handle, &key[..]).await })
}

/// content_type：S3 侧对象自身元数据负责 → 恒 None（空串）。key 校验语义与 core 对齐。
extern "C" fn content_type(_handle: u64, key: RString) -> FfiFuture {
    spawn_call(async move {
        os_path(&key[..])?;
        Ok(b"".to_vec())
    })
}

extern "C" fn close(handle: u64) {
    state().stores.lock().unwrap().remove(&handle);
}

static VTABLE: BlobBackendVtable = BlobBackendVtable {
    connect,
    put,
    get,
    del,
    url,
    content_type,
    close,
};

extern "C" fn register() -> PluginRegistrations {
    PluginRegistrations { es: std::ptr::null(), db: std::ptr::null(), blob: &VTABLE, bus: std::ptr::null() }
}

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("blob-s3"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        register,
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = (&host, &cfg); // blob 插件 init 无装配期配置（每后端 cfg 在 connect 传入）
    let st = BlobPluginState {
        rt: runtime(),
        stores: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
    };
    let _ = PLUGIN.set(st);
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-blob-s3 tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init);

#[cfg(test)]
mod tests {
    use super::*;

    /// cfg 校验离线路径：bucket/region 必填 fail-fast。
    #[test]
    fn build_store_requires_bucket_and_region() {
        let missing_bucket = S3Cfg {
            bucket: None,
            region: Some("us-east-1".into()),
            ..Default::default()
        };
        assert!(build_store(&missing_bucket).is_err());
        let missing_region = S3Cfg {
            bucket: Some("b".into()),
            region: None,
            ..Default::default()
        };
        assert!(build_store(&missing_region).is_err());
        // 仅 bucket+region 可离线构造（build 不触网）。
        let ok = S3Cfg {
            bucket: Some("my-bucket".into()),
            region: Some("us-east-1".into()),
            ..Default::default()
        };
        assert!(build_store(&ok).is_ok());
    }

    #[test]
    fn valid_key_rejects_traversal_and_absolute() {
        for bad in ["../x", "a/../b", "", "/abs", "a//b", "a\\b"] {
            assert!(!valid_key(bad), "{bad}");
        }
        assert!(valid_key("a/b.png"));
    }

    /// 真实 s3 e2e（env-gated）：`OJ_TEST_S3 = endpoint|bucket|region|access|secret|path_style`
    /// 未设置 → 跳过（不进网络）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_s3_roundtrip_via_vtable() {
        let Ok(dsn) = std::env::var("OJ_TEST_S3") else {
            eprintln!("skip: OJ_TEST_S3 unset");
            return;
        };
        let p: Vec<&str> = dsn.split('|').collect();
        assert!(p.len() >= 5, "OJ_TEST_S3 = endpoint|bucket|region|access|secret|path_style");
        let cfg = serde_json::json!({
            "driver": "s3",
            "root": "",
            "endpoint": p[0],
            "bucket": p[1],
            "region": p[2],
            "access_key": p[3],
            "secret_key": p[4],
            "path_style": p.get(5).map(|s| *s == "true").unwrap_or(true),
        })
        .to_string();
        let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", e[..].to_string()),
        };
        assert_eq!(&desc.name[..], "blob-s3");

        let bytes = drive(&mut connect(RString::from("default"), RString::from(cfg.as_str())))
            .await
            .expect("connect");
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        let key = format!("oj-test/{}.bin", std::process::id());
        drive(&mut put(
            handle,
            RString::from(key.as_str()),
            rbytes(b"hello-s3"),
            RString::from("application/octet-stream"),
        ))
        .await
        .expect("put");
        let got = drive(&mut get(handle, RString::from(key.as_str()))).await.expect("get");
        assert_eq!(got, b"hello-s3");
        let u = drive(&mut url(handle, RString::from(key.as_str()))).await.expect("url");
        assert!(String::from_utf8(u).unwrap().starts_with("http"), "presigned url");
        drive(&mut del(handle, RString::from(key.as_str()))).await.expect("del");
        assert!(drive(&mut get(handle, RString::from(key.as_str()))).await.is_err());

        close(handle);
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
extern "C" fn test_deliver(_topic: RString, _payload: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext { log: test_log, deliver: test_deliver })
    }

    fn rbytes(b: &[u8]) -> RBytes {
        let mut v = RBytes::new();
        for x in b {
            v.push(*x);
        }
        v
    }

    /// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
    async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
        for _ in 0..100_000 {
            match (fut.poll)(fut.state) {
                0 => tokio::task::yield_now().await,
                code => {
                    let r = (fut.take)(fut.state);
                    (fut.free)(fut.state);
                    fut.state = std::ptr::null_mut();
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Err(e)) => Err(e[..].to_string()),
                        _ => Err("ffi drive timeout".into()),
                    };
                }
            }
        }
        Err("ffi drive timeout".into())
    }
}
