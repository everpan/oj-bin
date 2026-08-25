//! blob 对象存储（OJ-5）：local/s3 双驱动统一 BlobBackend 契约 + key 防穿越。
//! JS 侧 `blob.put/get/del/url`（Extras 注入；未配置报 "blob not configured"）。
//! local 驱动的 content_type：object_store LocalFileSystem 不持久化 attributes——
//! 显式给的写 sidecar（`<key>.ct`），否则按扩展名推断。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use deno_core::{JsBuffer, OpState, op2};
use deno_error::JsErrorBox;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::signer::Signer;
use object_store::{ObjectStore, PutPayload};
use reqwest::Method;

use super::{BridgeResult, StableState};

/// blob 后端统一契约（接口隔离；local/s3 可替换）。
#[async_trait]
pub trait BlobBackend: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8], content_type: Option<&str>) -> BridgeResult<()>;
    async fn get(&self, key: &str) -> BridgeResult<Vec<u8>>;
    async fn del(&self, key: &str) -> BridgeResult<()>;
    /// 下载/外链地址（local = {base}/blob/{key}；s3 = presigned URL）。
    async fn url(&self, key: &str) -> BridgeResult<String>;
    async fn content_type(&self, key: &str) -> BridgeResult<Option<String>>;
    /// 下载路由直出：Some((bytes, content_type)) 或 302 Location。
    async fn serve(&self, key: &str) -> BridgeResult<BlobServed>;
}

/// serve 结果：内联直出或重定向（s3 presign）。
pub enum BlobServed {
    Bytes(Vec<u8>, Option<String>),
    Redirect(String),
}

/// key 白名单：'/' 分段，每段非空、非 `.`/`..`、不含 `\`/`\0`；整串非空、不以 `/` 开头。
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && key.split('/').all(|s| {
            !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', '\0'])
        })
}

fn os_path(key: &str) -> Result<Path, String> {
    valid_key(key).then(|| Path::from(key)).ok_or_else(|| format!("invalid blob key '{key}'"))
}

/// 扩展名 → Content-Type（下载路由用；罕见类型回落 octet-stream）。
fn infer_content_type(key: &str) -> Option<String> {
    let ext = key.rsplit('.').next()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "pdf" => "application/pdf",
            "txt" => "text/plain",
            "json" => "application/json",
            "js" | "mjs" => "text/javascript",
            "css" => "text/css",
            "html" | "htm" => "text/html",
            "mp4" => "video/mp4",
            "mp3" => "audio/mpeg",
            "zip" => "application/zip",
            "gz" => "application/gzip",
            _ => return None,
        }
        .to_string(),
    )
}

/// 本地文件系统驱动（object_store LocalFileSystem with_prefix）。
pub struct LocalBlob {
    store: LocalFileSystem,
    root: PathBuf,
    base_url: String,
    /// 注册名（spec §2：下载路由仅服务 "default"，非 default 的 url() 明确报错）。
    name: String,
}

impl LocalBlob {
    /// root 绝对/相对均可（调用方负责相对 config_dir 绝对化）；url 前缀 = {base}/blob。
    /// 等价 named("default", ...)（直构造不入注册表时保持路由可用语义）。
    pub fn new(root: &std::path::Path, base_url: &str) -> BridgeResult<Self> {
        Self::named("default", root, base_url)
    }

    /// 带注册名构造（装配层经 BlobRegistry::register 时透传注册名）。
    pub fn named(name: &str, root: &std::path::Path, base_url: &str) -> BridgeResult<Self> {
        std::fs::create_dir_all(root).map_err(|e| format!("blob root {}: {e}", root.display()))?;
        Ok(Self {
            store: LocalFileSystem::new_with_prefix(root).map_err(|e| format!("blob root {}: {e}", root.display()))?,
            root: root.to_path_buf(),
            base_url: base_url.trim_end_matches('/').to_string(),
            name: name.to_string(),
        })
    }

    /// sidecar 路径（content_type 持久化；local 专属）。
    fn ct_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.ct"))
    }
}

#[async_trait]
impl BlobBackend for LocalBlob {
    async fn put(&self, key: &str, bytes: &[u8], content_type: Option<&str>) -> BridgeResult<()> {
        let path = os_path(key)?;
        self.store
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .map_err(|e| format!("blob put: {e}"))?;
        // content_type sidecar（显式给的才写；推断可得的省略）。
        match content_type.filter(|ct| infer_content_type(key).as_deref() != Some(*ct)) {
            Some(ct) => {
                let p = self.ct_path(key);
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("blob ct dir: {e}"))?;
                }
                std::fs::write(p, ct).map_err(|e| format!("blob ct write: {e}"))?;
            }
            None => {
                let _ = std::fs::remove_file(self.ct_path(key));
            }
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> BridgeResult<Vec<u8>> {
        let path = os_path(key)?;
        let r = self.store.get(&path).await.map_err(|e| format!("blob get: {e}"))?;
        Ok(r.bytes().await.map_err(|e| format!("blob get: {e}"))?.to_vec())
    }

    async fn del(&self, key: &str) -> BridgeResult<()> {
        let path = os_path(key)?;
        // 幂等：key 不存在视为删除成功（object_store NotFound 吞掉）。
        match self.store.delete(&path).await {
            Ok(()) => {}
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(format!("blob del: {e}").into()),
        }
        let _ = std::fs::remove_file(self.ct_path(key));
        Ok(())
    }

    async fn url(&self, key: &str) -> BridgeResult<String> {
        os_path(key)?;
        if self.name != "default" {
            return Err(format!(
                "blob url() is only available for the 'default' backend (backend '{}': use get() or an s3 presign)",
                self.name
            )
            .into());
        }
        Ok(format!("{}/blob/{key}", self.base_url))
    }

    async fn content_type(&self, key: &str) -> BridgeResult<Option<String>> {
        os_path(key)?;
        Ok(std::fs::read_to_string(self.ct_path(key)).ok().filter(|s| !s.is_empty()).or_else(|| infer_content_type(key)))
    }

    async fn serve(&self, key: &str) -> BridgeResult<BlobServed> {
        Ok(BlobServed::Bytes(self.get(key).await?, self.content_type(key).await?))
    }
}

/// S3/MinIO 驱动（object_store AmazonS3 + Signer presign）。
/// url = GET presign 15min；serve = 302 重定向（S3 侧自带 Content-Type，无需 sidecar）。
pub struct S3Blob {
    store: object_store::aws::AmazonS3,
}

impl S3Blob {
    /// 从 config BlobCfg 装配：bucket/region 必填 fail-fast，endpoint/access_key/secret_key 可选。
    pub fn new(c: &crate::config::BlobCfg) -> BridgeResult<Self> {
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
        let mut b = object_store::aws::AmazonS3Builder::new()
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
        Ok(Self { store: b.build().map_err(|e| format!("blob s3 build: {e}"))? })
    }
}

#[async_trait]
impl BlobBackend for S3Blob {
    async fn put(&self, key: &str, bytes: &[u8], content_type: Option<&str>) -> BridgeResult<()> {
        let path = os_path(key)?;
        self.store
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .map_err(|e| format!("blob put: {e}"))?;
        // 忽略 content_type：presign GET 无法覆盖 S3 侧元数据——ponytail: s3 由对象自身元数据负责。
        let _ = content_type;
        Ok(())
    }

    async fn get(&self, key: &str) -> BridgeResult<Vec<u8>> {
        let path = os_path(key)?;
        let r = self.store.get(&path).await.map_err(|e| format!("blob get: {e}"))?;
        Ok(r.bytes().await.map_err(|e| format!("blob get: {e}"))?.to_vec())
    }

    async fn del(&self, key: &str) -> BridgeResult<()> {
        let path = os_path(key)?;
        match self.store.delete(&path).await {
            Ok(()) => {}
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(format!("blob del: {e}").into()),
        }
        Ok(())
    }

    async fn url(&self, key: &str) -> BridgeResult<String> {
        let path = os_path(key)?;
        self.store
            .signed_url(Method::GET, &path, std::time::Duration::from_secs(15 * 60))
            .await
            .map(|u| u.to_string())
            .map_err(|e| format!("blob s3 sign: {e}").into())
    }

    async fn content_type(&self, key: &str) -> BridgeResult<Option<String>> {
        os_path(key)?;
        Ok(None)
    }

    async fn serve(&self, key: &str) -> BridgeResult<BlobServed> {
        Ok(BlobServed::Redirect(self.url(key).await?))
    }
}

/// blob 轴注册表（键选式，命名多后端，spec §2）。注册全部发生在装配期（&mut self），装进 Arc 后不可变。
pub struct BlobRegistry {
    inner: crate::bridge::NamedRegistry<dyn BlobBackend>,
}

impl BlobRegistry {
    // 不走 derive(Default)：getter default() 与 Default::default() 撞名。
    pub fn new() -> Self {
        Self { inner: crate::bridge::NamedRegistry::new() }
    }
    /// 任意名字可注册；重名 fail fast（NamedRegistry 语义）。
    /// 配置声明了名字但装配时无对应后端 → 启动期报错（装配层职责，spec §2）。
    pub fn register(&mut self, name: &str, b: Arc<dyn BlobBackend>) -> BridgeResult<()> {
        self.inner.register(name, b)
    }
    pub fn default(&self) -> Option<Arc<dyn BlobBackend>> {
        self.inner.get("default")
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn BlobBackend>> {
        self.inner.get(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.inner.names().map(str::to_string).collect()
    }
}

/// 装配/测试共用：单个后端注册为 "default" 的注册表。
pub fn registry_with_default(b: Arc<dyn BlobBackend>) -> Arc<BlobRegistry> {
    let mut r = BlobRegistry::new();
    // 全新空注册表注册 default 必成功。
    r.register("default", b).unwrap();
    Arc::new(r)
}

/// 按名取后端（spec §2）：default 缺失保留旧文案；其余名字缺失报「blob backend '<name>'
/// not configured」（首次调用期报错——配置声明但装配失败在启动期已被 assemble_blobs 拦住）。
fn backend_named(state: &OpState, name: &str) -> Result<Arc<dyn BlobBackend>, JsErrorBox> {
    let reg = &state.borrow::<Arc<StableState>>().blobs;
    reg.get(name).ok_or_else(|| {
        JsErrorBox::generic(if name == "default" {
            "blob not configured (config blob: section missing)".to_string()
        } else {
            format!("blob backend '{name}' not configured (config blob.backends.{name} missing)")
        })
    })
}

/// blob.put(key, bytes, contentType?)。
#[op2]
pub async fn op_blob_put(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
    #[buffer] bytes: JsBuffer,
    #[string] content_type: Option<String>,
) -> Result<bool, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.put(&key, &bytes, content_type.as_deref())
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// blob.get(key) → Uint8Array。
#[op2]
#[buffer]
pub async fn op_blob_get(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> Result<Vec<u8>, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.get(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// blob.del(key)（幂等）。
#[op2]
pub async fn op_blob_del(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> Result<bool, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.del(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// blob.url(key) → 下载地址。
#[op2]
#[string]
pub async fn op_blob_url(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> Result<String, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.url(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// blob.contentType(key) → content-type 字符串；缺失/无 sidecar/无法推断扩展名时返回空串。
#[op2]
#[string]
pub async fn op_blob_content_type(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> Result<String, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    let ct = b
        .content_type(&key)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(ct.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #[test]
    fn blob_registry_multi_backend_and_duplicate_fails() {
        let root = std::env::temp_dir().join(format!("oj-blobreg-{}", std::process::id()));
        let mk = || Arc::new(LocalBlob::new(&root, "/v1/api").unwrap()) as Arc<dyn BlobBackend>;
        let mut r = BlobRegistry::new();
        assert!(r.default().is_none());
        r.register("default", mk()).unwrap();
        r.register("img", mk()).unwrap();
        assert!(r.default().is_some());
        assert!(r.get("img").is_some());
        assert_eq!(r.names(), vec!["default".to_string(), "img".to_string()]);
        // 重名 fail fast
        assert!(r.register("img", mk()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    use super::*;

    use serde_json::Value;

    use crate::bridge::{BlobServed, Bridge, Extras, InMemoryKV, RequestInfo, S3Blob, SchemaRegistry};
    use crate::config::BlobCfg;

    fn tmp_root() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "oj-blob-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_roundtrip_and_traversal_rejected() {
        let root = tmp_root();
        let b = LocalBlob::new(&root, "/v1/api").unwrap();
        b.put("a/b.png", b"PNGDATA", Some("image/png")).await.unwrap();
        assert_eq!(b.get("a/b.png").await.unwrap(), b"PNGDATA".to_vec());
        assert_eq!(b.url("a/b.png").await.unwrap(), "/v1/api/blob/a/b.png");
        assert_eq!(b.content_type("a/b.png").await.unwrap().as_deref(), Some("image/png"));
        // 显式非常规 ct 走 sidecar；无 ct 回落扩展名推断
        b.put("x.bin", b"B", Some("application/x-foo")).await.unwrap();
        assert_eq!(b.content_type("x.bin").await.unwrap().as_deref(), Some("application/x-foo"));
        b.put("y.png", b"P", None).await.unwrap();
        assert_eq!(b.content_type("y.png").await.unwrap().as_deref(), Some("image/png"));
        b.del("a/b.png").await.unwrap();
        assert!(b.get("a/b.png").await.is_err());
        for bad in ["../x", "a/../b", "", "/abs", "a//b", "a\\b"] {
            assert!(!valid_key(bad), "{bad}");
            assert!(b.put(bad, b"x", None).await.is_err(), "{bad}");
        }
    }

    /// S3/MinIO e2e：env `OJ_TEST_S3` = `endpoint|bucket|region|access|secret|path_style`
    /// （path_style 缺省 true，适配 MinIO）。未设置 → 跳过。
    #[tokio::test]
    #[ignore]
    async fn s3_e2e_roundtrip() {
        let Ok(dsn) = std::env::var("OJ_TEST_S3") else {
            eprintln!("skip: OJ_TEST_S3 unset");
            return;
        };
        let p: Vec<&str> = dsn.split('|').collect();
        assert!(p.len() >= 5, "OJ_TEST_S3 = endpoint|bucket|region|access|secret|path_style");
        let cfg = crate::config::BlobCfg {
            driver: "s3".into(),
            root: String::new(),
            endpoint: Some(p[0].into()),
            bucket: Some(p[1].into()),
            region: Some(p[2].into()),
            access_key: Some(p[3].into()),
            secret_key: Some(p[4].into()),
            path_style: p.get(5).map(|s| *s == "true").unwrap_or(true),
        };
        let b = S3Blob::new(&cfg).unwrap();
        let key = format!("oj-test/{}.bin", std::process::id());
        b.put(&key, b"hello-s3", Some("application/octet-stream")).await.unwrap();
        assert_eq!(b.get(&key).await.unwrap(), b"hello-s3".to_vec());
        assert!(b.url(&key).await.unwrap().starts_with("http"), "presigned url");
        b.del(&key).await.unwrap();
        assert!(b.get(&key).await.is_err());
    }

    #[test]
    fn infer_content_type_by_extension() {
        assert_eq!(infer_content_type("a.png"), Some("image/png".into()));
        assert_eq!(infer_content_type("a.JPG"), Some("image/jpeg".into()));
        assert_eq!(infer_content_type("a.svg"), Some("image/svg+xml".into()));
        assert_eq!(infer_content_type("a.pdf"), Some("application/pdf".into()));
        assert_eq!(infer_content_type("a.json"), Some("application/json".into()));
        assert_eq!(infer_content_type("a.mp4"), Some("video/mp4".into()));
        assert_eq!(infer_content_type("a.unknown"), None);
    }

    #[test]
    fn s3_new_requires_bucket_and_region() {
        let missing_bucket = crate::config::BlobCfg {
            driver: "s3".into(),
            root: String::new(),
            endpoint: Some("http://127.0.0.1:9000".into()),
            bucket: None,
            region: Some("us-east-1".into()),
            access_key: None,
            secret_key: None,
            path_style: true,
        };
        assert!(S3Blob::new(&missing_bucket).is_err());
        let missing_region = crate::config::BlobCfg {
            driver: "s3".into(),
            root: String::new(),
            endpoint: Some("http://127.0.0.1:9000".into()),
            bucket: Some("b".into()),
            region: None,
            access_key: None,
            secret_key: None,
            path_style: true,
        };
        assert!(S3Blob::new(&missing_region).is_err());
    }

    /// 非 default local 后端 url() 明确报错（下载路由仅服务 default，spec §2 裁决）。
    #[tokio::test(flavor = "current_thread")]
    async fn local_blob_url_errors_when_not_default() {
        let root = tmp_root();
        let b = LocalBlob::named("img", &root, "/v1/api").unwrap();
        let e = b.url("k").await.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains("only available for the 'default' backend"), "{e}");
        // default 名不受影响
        let d = LocalBlob::new(&root, "/v1/api").unwrap();
        assert_eq!(d.url("k").await.unwrap(), "/v1/api/blob/k");
    }

    /// blob(name) 工厂：命名分发 + default 兼容旧调用 + 未配置名首次调用期报错（spec §2）。
    #[tokio::test(flavor = "current_thread")]
    async fn blob_named_factory_dispatch() {
        let root_d = tmp_root();
        let root_i = tmp_root();
        let mut reg = BlobRegistry::new();
        reg.register("default", Arc::new(LocalBlob::new(&root_d, "/v1/api").unwrap())).unwrap();
        reg.register("img", Arc::new(LocalBlob::new(&root_i, "/v1/api").unwrap())).unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras { blobs: Some(Arc::new(reg)), ..Default::default() },
        );
        // 命名分发：img 与 default 互不串（同名 key 不同内容）
        let cap = b
            .run_with(
                r#"
                (async () => {
                    await blob("img").put("k.txt", new Uint8Array([73, 77, 71]), "text/plain");
                    await blob.put("k.txt", new Uint8Array([68, 69, 70]), "text/plain");
                    const img = Array.from(await blob("img").get("k.txt")).join(",");
                    const def = Array.from(await blob.get("k.txt")).join(",");
                    json.ok({ img, def });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["img"], "73,77,71", "{v}");
        assert_eq!(v["data"]["def"], "68,69,70", "{v}");
        assert!(root_i.join("k.txt").is_file() && root_d.join("k.txt").is_file());
        // 未配置名：首次调用期报错（name 入文案）
        let cap = b
            .run_with(
                r#"(async () => { await blob("ghost").get("k"); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["err"].as_str().unwrap().contains("blob backend 'ghost' not configured"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_serve_returns_bytes_and_content_type() {
        let root = tmp_root();
        let b = LocalBlob::new(&root, "/v1/api").unwrap();
        b.put("d/e.txt", b"hello", Some("text/plain")).await.unwrap();
        // serve 直出字节 + content_type
        let sv = b.serve("d/e.txt").await.unwrap();
        match sv {
            BlobServed::Bytes(bytes, ct) => {
                assert_eq!(bytes, b"hello".to_vec());
                assert_eq!(ct.as_deref(), Some("text/plain"));
            }
            BlobServed::Redirect(_) => panic!("local blob must inline-serve"),
        }
        // 越界 key 经 os_path 拒绝
        assert!(b.serve("../up").await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s3_new_success_and_content_type_offline() {
        // S3Blob::new 成功路径（仅需 bucket/region；离线可构造，build 不触网）。
        let cfg = BlobCfg {
            driver: "s3".into(),
            root: String::new(),
            endpoint: None,
            bucket: Some("my-bucket".into()),
            region: Some("us-east-1".into()),
            access_key: None,
            secret_key: None,
            path_style: false,
        };
        let s3 = S3Blob::new(&cfg).unwrap();
        // content_type：S3 侧自身元数据负责 → None（不触网）。
        assert_eq!(s3.content_type("a/b.png").await.unwrap(), None);
        // 非法 key（穿越/绝对路径）被 os_path 拒绝。
        assert!(s3.content_type("/abs").await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn op_blob_content_type_via_bridge() {
        let root = tmp_root();
        let local = LocalBlob::new(&root, "/v1/api").unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras { blobs: Some(registry_with_default(Arc::new(local))), ..Default::default() },
        );
        let cap = b
            .run_with(
                r#"
                (async () => {
                    await blob.put("js/a.bin", new Uint8Array([1]), "application/x-foo");
                    const explicit = await blob.contentType("js/a.bin");     // sidecar
                    const inferred = await blob.contentType("js/b.png");    // 未见过的 key → 扩展名推断
                    const missing = await blob.contentType("js/nope");      // 无 sidecar / 无扩展名 → null
                    json.ok({ explicit, inferred, missing });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["explicit"], "application/x-foo");
        assert_eq!(v["data"]["inferred"], "image/png");
        assert_eq!(v["data"]["missing"], "");
    }
}
