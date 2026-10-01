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
    ABI_VERSION, BlobBackendVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RBytes,
    RResult, RString,
};
use reqwest::Method;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 插件侧配置视图（= core config::BlobCfg 的 JSON；serde 只取插件关心的字段）。
#[derive(Deserialize, Default)]
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

/// 插件共享状态（进程级单例，init 建立）。
struct BlobPluginState {
    rt: tokio::runtime::Runtime,
    stores: Mutex<HashMap<u64, Arc<AmazonS3>>>,
    /// ABI 10 流式上传会话（upload_id → 会话；会话体 tokio Mutex 独占，chunk 跨 await）。
    uploads: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<PutStream>>>>,
    next_handle: AtomicU64,
    next_upload: AtomicU64,
}

/// 流式上传会话：object_store MultipartUpload + ≥8 MiB 定长 part 缓冲
/// （S3 除末块外要求 ≥5 MiB，R2 类还要求等长——定长 8 MiB 兼容两者）。
struct PutStream {
    upload: Box<dyn object_store::MultipartUpload>,
    buf: Vec<u8>,
}

/// 单 part 大小（S3 下限 5 MiB；8 MiB 留出等长兼容余量）。
const PART_SIZE: usize = 8 * 1024 * 1024;

static PLUGIN: OnceLock<BlobPluginState> = OnceLock::new();

fn state() -> &'static BlobPluginState {
    PLUGIN.get().expect("oj-blob-s3: init not called")
}

// ---- FfiFuture 桥（统一走 oj-plugin-ffi 的 catch_unwind 安全工厂：spawn_ffi_future / catch_future）----

// ---- s3 逻辑（迁自 core S3Blob + blob.rs 的 key 校验，语义对齐）----

/// key 白名单：'/' 分段，每段非空、非 `.`/`..`、不含 `\`/`\0`；整串非空、不以 `/` 开头。
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && key
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', '\0']))
}

fn os_path(key: &str) -> Result<Path, String> {
    valid_key(key)
        .then(|| Path::from(key))
        .ok_or_else(|| format!("invalid blob key '{key}'"))
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
        let r = self
            .store(handle)?
            .get(&path)
            .await
            .map_err(|e| format!("blob get: {e}"))?;
        Ok(r.bytes()
            .await
            .map_err(|e| format!("blob get: {e}"))?
            .to_vec())
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

    /// ABI 9 上传直传预签名。object_store 的 `signed_url` 覆盖单发 PUT；
    /// multipart 预签名（Create/UploadPart/Complete）不在 object_store API 面内——
    /// ponytail：单发 PUT 已解决 10MB/30s 上限（直传不进 handler），multipart 预留 op
    /// 语义、当前返回 Err（>100MB 超大文件待真实需求再手写 SigV4 三段预签名）。
    async fn do_upload_url(&self, handle: u64, key: &str, op: &str) -> Result<Vec<u8>, String> {
        let path = os_path(key)?;
        let v: serde_json::Value =
            serde_json::from_str(op).map_err(|e| format!("blob s3 upload_url op: {e}"))?;
        let kind = v["kind"].as_str().unwrap_or("");
        if kind != "put" {
            return Err(format!(
                "blob s3 upload_url: multipart '{kind}' presign not supported"
            ));
        }
        let u = self
            .store(handle)?
            .signed_url(Method::PUT, &path, std::time::Duration::from_secs(15 * 60))
            .await
            .map_err(|e| format!("blob s3 sign: {e}"))?
            .to_string();
        Ok(format!(r#"{{"url":{u:?}}}"#).into_bytes())
    }

    // ---- ABI 10：流式上传（multipart：每满 8 MiB 一个 part；finish 收尾 part + complete；
    // abort 取消 multipart——S3 不清理已传 part，不 abort 会 orphan parts 烧钱）----

    async fn do_put_stream_open(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        let path = os_path(key)?;
        let store = self.store(handle)?;
        let upload = store
            .put_multipart(&path)
            .await
            .map_err(|e| format!("blob put_stream_open: {e}"))?;
        let id = self.next_upload.fetch_add(1, Ordering::SeqCst) + 1;
        self.uploads.lock().unwrap().insert(
            id,
            Arc::new(tokio::sync::Mutex::new(PutStream {
                upload,
                buf: Vec::new(),
            })),
        );
        Ok(format!(r#"{{"upload_id":{id}}}"#).into_bytes())
    }

    async fn do_put_stream_chunk(
        &self,
        handle: u64,
        upload_id: u64,
        bytes: &[u8],
    ) -> Result<Vec<u8>, String> {
        let s = {
            let m = self.uploads.lock().unwrap();
            m.get(&upload_id)
                .cloned()
                .ok_or_else(|| format!("blob put_stream_chunk: unknown upload {upload_id}"))?
        };
        let mut g = s.lock().await;
        g.buf.extend_from_slice(bytes);
        // 定长出块：凑满 8 MiB 即推一个 part（剩余尾量留给后续 chunk / finish）。
        while g.buf.len() >= PART_SIZE {
            let chunk: Vec<u8> = g.buf.drain(..PART_SIZE).collect();
            g.upload
                .put_part(chunk.into())
                .await
                .map_err(|e| format!("blob put_stream_chunk: {e}"))?;
        }
        let _ = handle;
        Ok(b"".to_vec())
    }

    async fn do_put_stream_finish(&self, handle: u64, upload_id: u64) -> Result<Vec<u8>, String> {
        let s = {
            let mut m = self.uploads.lock().unwrap();
            m.remove(&upload_id)
                .ok_or_else(|| format!("blob put_stream_finish: unknown upload {upload_id}"))?
        };
        let mut g = s.lock().await;
        // 尾块（< 8 MiB）作为最后 part 推出，再 complete 使对象原子可见。
        if !g.buf.is_empty() {
            let rest = std::mem::take(&mut g.buf);
            g.upload
                .put_part(rest.into())
                .await
                .map_err(|e| format!("blob put_stream_finish: {e}"))?;
        }
        g.upload
            .complete()
            .await
            .map_err(|e| format!("blob put_stream_finish: {e}"))?;
        let _ = handle;
        Ok(b"".to_vec())
    }

    async fn do_put_stream_abort(&self, handle: u64, upload_id: u64) -> Result<Vec<u8>, String> {
        let s = {
            let mut m = self.uploads.lock().unwrap();
            m.remove(&upload_id)
                .ok_or_else(|| format!("blob put_stream_abort: unknown upload {upload_id}"))?
        };
        let mut g = s.lock().await;
        g.upload
            .abort()
            .await
            .map_err(|e| format!("blob put_stream_abort: {e}"))?;
        let _ = handle;
        Ok(b"".to_vec())
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
        // object_store 默认 allow_http=false → reqwest 客户端 https_only(true)，
        // 明文端点会在「建请求」阶段被拒（builder error for url，0 retries）。
        // sample/config.yaml 的 MinIO 示例就是 http 端点 → 按 scheme 显式放开。
        if e.starts_with("http://") {
            b = b.with_allow_http(true);
        }
        b = b.with_endpoint(e);
    }
    if let Some(k) = c.access_key.as_deref().filter(|s| !s.is_empty()) {
        b = b.with_access_key_id(k);
    }
    if let Some(k) = c.secret_key.as_deref().filter(|s| !s.is_empty()) {
        b = b.with_secret_access_key(k);
    }
    Ok(Arc::new(
        b.build().map_err(|e| format!("blob s3 build: {e}"))?,
    ))
}

// ---- vtable（同步签名返回 FfiFuture；connect 产 handle，close 释放）----

extern "C" fn connect(name: RString, cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: S3Cfg =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("blob s3: bad cfg: {e}"))?;
            let store = build_store(&cfg)?;
            let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
            st.stores.lock().unwrap().insert(handle, store);
            let _ = &name; // 注册名透传（url 裁决保留签名）；s3 presign 对所有名字可用
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

extern "C" fn put(handle: u64, key: RString, bytes: RBytes, _content_type: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let mut b = Vec::with_capacity(bytes.len());
        for x in &bytes {
            b.push(*x);
        }
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { st.do_put(handle, &key[..], &b).await },
        )
    })
}

extern "C" fn get(handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { st.do_get(handle, &key[..]).await })
    })
}

extern "C" fn del(handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { st.do_del(handle, &key[..]).await })
    })
}

extern "C" fn url(handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { st.do_url(handle, &key[..]).await })
    })
}

/// ABI 9 上传直传预签名（put / multipart 四态，op JSON 语义见 oj-plugin-ffi blob.rs）。
extern "C" fn upload_url(handle: u64, key: RString, op: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_upload_url(handle, &key[..], &op[..]).await
        })
    })
}

/// content_type：S3 侧对象自身元数据负责 → 恒 None（空串）。key 校验语义与 core 对齐。
extern "C" fn content_type(_handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(&state().rt, async move {
            os_path(&key[..])?;
            Ok(b"".to_vec())
        })
    })
}

// ---- ABI 10：流式上传（open/chunk/finish/abort）----

extern "C" fn put_stream_open(handle: u64, key: RString, _content_type: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_put_stream_open(handle, &key[..]).await
        })
    })
}

extern "C" fn put_stream_chunk(handle: u64, upload_id: u64, bytes: RBytes) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let mut b = Vec::with_capacity(bytes.len());
        for x in &bytes {
            b.push(*x);
        }
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_put_stream_chunk(handle, upload_id, &b).await
        })
    })
}

extern "C" fn put_stream_finish(handle: u64, upload_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_put_stream_finish(handle, upload_id).await
        })
    })
}

extern "C" fn put_stream_abort(handle: u64, upload_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_put_stream_abort(handle, upload_id).await
        })
    })
}

extern "C" fn close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        state().stores.lock().unwrap().remove(&handle);
    })
}

static VTABLE: BlobBackendVtable = BlobBackendVtable {
    connect,
    put,
    get,
    del,
    url,
    upload_url,
    content_type,
    close,
    put_stream_open,
    put_stream_chunk,
    put_stream_finish,
    put_stream_abort,
};

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("blob-s3"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "blob 轴 s3 cdylib 插件：object_store AmazonS3 迁自 core S3Blob（Task 4.2）",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = (&host, &cfg); // blob 插件 init 无装配期配置（每后端 cfg 在 connect 传入）
    // get_or_init：并发 init 时闭包只跑一次（竞争方阻塞复用），不重复建 runtime，
    // 避免 `let _ = set(st)` 在竞争下把败者的 tokio Runtime 从 async 上下文 drop 崩溃。
    PLUGIN.get_or_init(|| BlobPluginState {
        rt: runtime(),
        stores: Mutex::new(HashMap::new()),
        uploads: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
        next_upload: AtomicU64::new(0),
    });
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-blob-s3 tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, blob => &VTABLE);

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

    /// http 端点的离线回归闸：明文端点必须走到**网络**阶段（连不上 → 报错里含 connect/sending），
    /// 而不是「建请求」阶段被拒。后者 = object_store 默认 `allow_http=false` →
    /// reqwest `https_only(true)` → `builder error for url`，正是 MinIO 明文端点全废的那个
    /// 100% 断链（详见 plane/apps/api-oj/docs/poc-report.md §G④-a）。
    /// 不联网：打本机一个必然关闭的端口，只断言错误**类别**。
    #[tokio::test]
    async fn http_endpoint_reaches_network_not_url_builder() {
        let cfg = S3Cfg {
            driver: "s3".into(),
            endpoint: Some("http://127.0.0.1:1".into()),
            bucket: Some("b".into()),
            region: Some("us-east-1".into()),
            access_key: Some("k".into()),
            secret_key: Some("s".into()),
            path_style: true,
            ..Default::default()
        };
        let store = build_store(&cfg).expect("build");
        let err = store
            .put(&os_path("x.txt").unwrap(), PutPayload::from(b"x".to_vec()))
            .await
            .expect_err("closed port must fail")
            .to_string();
        assert!(
            !err.contains("builder error"),
            "http 端点被建请求阶段拒绝（allow_http 未放开）: {err}"
        );
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
        assert!(
            p.len() >= 5,
            "OJ_TEST_S3 = endpoint|bucket|region|access|secret|path_style"
        );
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
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "blob-s3");

        let bytes = drive(&mut connect(
            RString::from("default"),
            RString::from(cfg.as_str()),
        ))
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
        let got = drive(&mut get(handle, RString::from(key.as_str())))
            .await
            .expect("get");
        assert_eq!(got, b"hello-s3");
        let u = drive(&mut url(handle, RString::from(key.as_str())))
            .await
            .expect("url");
        assert!(
            String::from_utf8(u).unwrap().starts_with("http"),
            "presigned url"
        );
        drive(&mut del(handle, RString::from(key.as_str())))
            .await
            .expect("del");
        assert!(
            drive(&mut get(handle, RString::from(key.as_str())))
                .await
                .is_err()
        );

        close(handle);
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        })
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
        // 以真实墙钟时间为界轮询（同 oj-es 的 drive）：固定 10w 次 yield_now 在 CI
        // 负载/优化下会在插件 rt 的任务完成前耗尽预算，误报 "ffi drive timeout"。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match (fut.poll)(fut.state) {
                0 => {
                    if std::time::Instant::now() >= deadline {
                        (fut.free)(fut.state); // 超时也要释放 state（防 FfiTask 泄漏）
                        fut.state = std::ptr::null_mut();
                        return Err("ffi drive timeout".into());
                    }
                    tokio::time::sleep(std::time::Duration::from_micros(100)).await;
                }
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
    }
}
