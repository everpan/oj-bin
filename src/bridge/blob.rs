//! blob 对象存储（OJ-5）：local/s3 双驱动统一 BlobBackend 契约 + key 防穿越。
//! JS 侧 `blob.put/get/del/url`（Extras 注入；未配置报 "blob not configured"）。
//! local 驱动的 content_type：object_store LocalFileSystem 不持久化 attributes——
//! 显式给的写 sidecar（`<key>.ct`），否则按扩展名推断。

#![allow(
    clippy::new_without_default,
    clippy::collapsible_if,
    clippy::redundant_closure,
    clippy::type_complexity
)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use deno_core::{JsBuffer, OpState, op2};
use deno_error::JsErrorBox;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{ObjectStoreExt, PutPayload};

use super::{BridgeResult, StableState};

/// blob 后端统一契约（接口隔离；local/s3 可替换）。
#[async_trait]
pub trait BlobBackend: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8], content_type: Option<&str>) -> BridgeResult<()>;
    async fn get(&self, key: &str) -> BridgeResult<Vec<u8>>;
    async fn del(&self, key: &str) -> BridgeResult<()>;
    /// 下载/外链地址（local = {base}/blob/{key}；s3 = presigned URL）。
    async fn url(&self, key: &str) -> BridgeResult<String>;
    /// 上传直传预签名（ABI 9；op JSON 语义同插件契约）。local 无预签名概念：
    /// 返回 Err，宿主回落直传 PUT 路由。
    async fn upload_url(&self, key: &str, op: &str) -> BridgeResult<serde_json::Value>;
    async fn content_type(&self, key: &str) -> BridgeResult<Option<String>>;
    /// 下载路由直出：Some((bytes, content_type)) 或 302 Location。
    async fn serve(&self, key: &str) -> BridgeResult<BlobServed>;
    /// ---- ABI 10 流式上传（put_stream_*；服务端流式 multipart / PUT 直传落盘）----
    /// 开会话：返回 upload_id。不支持的后端返回 Err（宿主回落整体缓冲 put）。
    async fn put_stream_open(&self, _key: &str, _content_type: Option<&str>) -> BridgeResult<u64> {
        Err("blob backend does not support streaming upload (ABI 10 required)".into())
    }
    /// 追加一块字节。
    async fn put_stream_chunk(&self, _upload_id: u64, _bytes: &[u8]) -> BridgeResult<()> {
        Err("blob backend does not support streaming upload (ABI 10 required)".into())
    }
    /// 提交（local = 临时文件转正 + ct sidecar；s3 = complete multipart）。
    async fn put_stream_finish(&self, _upload_id: u64) -> BridgeResult<()> {
        Err("blob backend does not support streaming upload (ABI 10 required)".into())
    }
    /// 失败清理（s3 abort multipart 防 orphan parts；local 删临时文件）。幂等友好。
    async fn put_stream_abort(&self, _upload_id: u64) -> BridgeResult<()> {
        Err("blob backend does not support streaming upload (ABI 10 required)".into())
    }
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
        && key
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', '\0']))
}

fn os_path(key: &str) -> Result<Path, String> {
    valid_key(key)
        .then(|| Path::from(key))
        .ok_or_else(|| format!("invalid blob key '{key}'"))
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
    /// 流式上传会话（ABI 10；upload_id → 会话）。会话体经 tokio Mutex 独占
    /// （chunk 写盘跨 await，std MutexGuard 不可过界）；std Mutex 只护表本身。
    uploads: std::sync::Mutex<HashMap<u64, Arc<tokio::sync::Mutex<LocalPutSession>>>>,
    next_upload: std::sync::atomic::AtomicU64,
}

/// LocalBlob 流式上传会话：写 `<root>/.oj-tmp-upload-{pid}-{id}`，finish rename 转正。
/// **Drop 守卫**：finish 未跑（abort / 宿主崩溃除外——崩溃留残骸由部署清理）即删临时文件。
struct LocalPutSession {
    tmp: PathBuf,
    final_path: PathBuf,
    key: String,
    ct: Option<String>,
    /// Some = 仍在写入；finish/abort 时 take（LocalPutSession 实现 Drop，不能 move 出字段）。
    file: Option<tokio::fs::File>,
    finished: bool,
}

impl Drop for LocalPutSession {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
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
            store: LocalFileSystem::new_with_prefix(root)
                .map_err(|e| format!("blob root {}: {e}", root.display()))?,
            root: root.to_path_buf(),
            base_url: base_url.trim_end_matches('/').to_string(),
            name: name.to_string(),
            uploads: std::sync::Mutex::new(HashMap::new()),
            next_upload: std::sync::atomic::AtomicU64::new(0),
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
        let r = self
            .store
            .get(&path)
            .await
            .map_err(|e| format!("blob get: {e}"))?;
        Ok(r.bytes()
            .await
            .map_err(|e| format!("blob get: {e}"))?
            .to_vec())
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

    async fn upload_url(&self, key: &str, _op: &str) -> BridgeResult<serde_json::Value> {
        os_path(key)?;
        Err("local blob backend has no upload presign; use the direct PUT upload route".into())
    }

    async fn content_type(&self, key: &str) -> BridgeResult<Option<String>> {
        os_path(key)?;
        Ok(std::fs::read_to_string(self.ct_path(key))
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| infer_content_type(key)))
    }

    async fn serve(&self, key: &str) -> BridgeResult<BlobServed> {
        Ok(BlobServed::Bytes(
            self.get(key).await?,
            self.content_type(key).await?,
        ))
    }

    /// 流式上传（ABI 10）：临时文件 append → finish rename 转正 + ct sidecar。
    /// 临时文件名带 pid+序号防并发互踩；Drop 守卫兜底清理（finish/abort 漏调也不泄漏）。
    async fn put_stream_open(&self, key: &str, content_type: Option<&str>) -> BridgeResult<u64> {
        os_path(key)?;
        let id = self
            .next_upload
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let tmp = self
            .root
            .join(format!(".oj-tmp-upload-{}-{}", std::process::id(), id));
        let file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| format!("blob put_stream_open: {e}"))?;
        self.uploads.lock().unwrap().insert(
            id,
            Arc::new(tokio::sync::Mutex::new(LocalPutSession {
                tmp: tmp.clone(),
                final_path: self.root.join(key),
                key: key.to_string(),
                ct: content_type.map(str::to_string),
                file: Some(file),
                finished: false,
            })),
        );
        Ok(id)
    }

    async fn put_stream_chunk(&self, upload_id: u64, bytes: &[u8]) -> BridgeResult<()> {
        use tokio::io::AsyncWriteExt;
        let s = {
            let m = self.uploads.lock().unwrap();
            m.get(&upload_id)
                .cloned()
                .ok_or_else(|| format!("blob put_stream_chunk: unknown upload {upload_id}"))?
        };
        let mut g = s.lock().await;
        g.file
            .as_mut()
            .ok_or_else(|| format!("blob put_stream_chunk: upload {upload_id} already closed"))?
            .write_all(bytes)
            .await
            .map_err(|e| format!("blob put_stream_chunk: {e}"))?;
        Ok(())
    }

    async fn put_stream_finish(&self, upload_id: u64) -> BridgeResult<()> {
        use tokio::io::AsyncWriteExt;
        let s = {
            let mut m = self.uploads.lock().unwrap();
            m.remove(&upload_id)
                .ok_or_else(|| format!("blob put_stream_finish: unknown upload {upload_id}"))?
        };
        let mut g = s.lock().await;
        let mut file = g
            .file
            .take()
            .ok_or_else(|| format!("blob put_stream_finish: upload {upload_id} already closed"))?;
        file.flush()
            .await
            .map_err(|e| format!("blob put_stream_finish: {e}"))?;
        file.sync_all()
            .await
            .map_err(|e| format!("blob put_stream_finish: {e}"))?;
        drop(file);
        // 目标父目录可能不存在（object_store put 自建目录；rename 不会）→ 先建。
        if let Some(dir) = g.final_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("blob put_stream_finish: {e}"))?;
        }
        std::fs::rename(&g.tmp, &g.final_path)
            .map_err(|e| format!("blob put_stream_finish: {e}"))?;
        // ct sidecar：与 put() 同语义（显式给的且与推断不同才写；否则清掉旧 sidecar）。
        let key = g.key.clone();
        match g
            .ct
            .as_deref()
            .filter(|ct| infer_content_type(&key).as_deref() != Some(*ct))
        {
            Some(ct) => {
                let p = self.ct_path(&key);
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("blob ct dir: {e}"))?;
                }
                std::fs::write(p, ct).map_err(|e| format!("blob ct write: {e}"))?;
            }
            None => {
                let _ = std::fs::remove_file(self.ct_path(&key));
            }
        }
        g.finished = true; // Drop 守卫不再删（tmp 已 rename 走，此处防御语义）
        Ok(())
    }

    async fn put_stream_abort(&self, upload_id: u64) -> BridgeResult<()> {
        // remove 后（在途 chunk 释放 Arc 时）Drop 守卫删临时文件；未知 id 幂等成功。
        let s = self.uploads.lock().unwrap().remove(&upload_id);
        if let Some(s) = s {
            let mut g = s.lock().await;
            g.file.take(); // 先关句柄；finished 保持 false → 会话释放时 Drop 守卫删 tmp
        }
        Ok(())
    }
}

// S3 驱动 Task 4.2 迁出：`oj-blob-s3` cdylib 插件承载（core 只留 LocalBlob）。

/// blob 轴注册表（键选式，命名多后端，spec §2）。注册全部发生在装配期（&mut self），装进 Arc 后不可变。
pub struct BlobRegistry {
    inner: crate::bridge::NamedRegistry<dyn BlobBackend>,
}

#[allow(clippy::new_without_default)]
impl BlobRegistry {
    // 不走 derive(Default)：getter default() 与 Default::default() 撞名。
    pub fn new() -> Self {
        Self {
            inner: crate::bridge::NamedRegistry::new(),
        }
    }
    /// 任意名字可注册；重名 fail fast（NamedRegistry 语义）。
    /// 配置声明了名字但装配时无对应后端 → 启动期报错（装配层职责，spec §2）。
    pub fn register(&mut self, name: &str, b: Arc<dyn BlobBackend>) -> BridgeResult<()> {
        self.inner.register(name, b)
    }
    /// 设置默认别名：字面 `blob("default")` 解析到 `name`（CLI `--blob` 选源）。
    /// `name` 不存在 → fail-fast。
    pub fn set_default_alias(&mut self, name: &str) -> BridgeResult<()> {
        self.inner.set_default_alias(name)
    }
    pub fn default(&self) -> Option<Arc<dyn BlobBackend>> {
        self.inner.default()
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

/// 按名取后端（spec §2）：default 经别名解析（CLI `--blob` 选源）；default 缺失保留旧文案；
/// 其余名字缺失报「blob backend '<name>' not configured」。
fn backend_named(state: &OpState, name: &str) -> Result<Arc<dyn BlobBackend>, JsErrorBox> {
    let reg = &state.borrow::<Arc<StableState>>().blobs;
    if name == "default" {
        return reg.default().ok_or_else(|| {
            JsErrorBox::generic("blob not configured (config blob: section missing)".to_string())
        });
    }
    reg.get(name).ok_or_else(|| {
        JsErrorBox::generic(format!(
            "blob backend '{name}' not configured (config blob.backends.{name} missing)"
        ))
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
    b.get(&key)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// blob.del(key)（幂等）。
#[op2]
pub async fn op_blob_del(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> Result<bool, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.del(&key)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
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
    b.url(&key)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// blob.uploadUrl(key, opts?) → 上传直传预签名 JSON（v0.1.30，ABI 9）。
/// opts 缺省 `{"kind":"put"}`；s3 后端返回预签名 PUT URL（15min），local 后端抛错
/// （"local blob backend has no upload presign; use the direct PUT upload route"——
/// 直传用 `PUT {base}/blob/{key}` 路由）。
#[op2]
#[string]
pub async fn op_blob_upload_url(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
    #[string] op: Option<String>,
) -> Result<String, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    let v = b
        .upload_url(&key, op.as_deref().unwrap_or(r#"{"kind":"put"}"#))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    serde_json::to_string(&v).map_err(|e| JsErrorBox::generic(e.to_string()))
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

    use crate::bridge::{BlobServed, Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    // S3 配置测试随 S3Blob 迁插件（oj-blob-s3，Task 4.2）；core 不再引用 BlobCfg。

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
        b.put("a/b.png", b"PNGDATA", Some("image/png"))
            .await
            .unwrap();
        assert_eq!(b.get("a/b.png").await.unwrap(), b"PNGDATA".to_vec());
        assert_eq!(b.url("a/b.png").await.unwrap(), "/v1/api/blob/a/b.png");
        assert_eq!(
            b.content_type("a/b.png").await.unwrap().as_deref(),
            Some("image/png")
        );
        // 显式非常规 ct 走 sidecar；无 ct 回落扩展名推断
        b.put("x.bin", b"B", Some("application/x-foo"))
            .await
            .unwrap();
        assert_eq!(
            b.content_type("x.bin").await.unwrap().as_deref(),
            Some("application/x-foo")
        );
        b.put("y.png", b"P", None).await.unwrap();
        assert_eq!(
            b.content_type("y.png").await.unwrap().as_deref(),
            Some("image/png")
        );
        b.del("a/b.png").await.unwrap();
        assert!(b.get("a/b.png").await.is_err());
        for bad in ["../x", "a/../b", "", "/abs", "a//b", "a\\b"] {
            assert!(!valid_key(bad), "{bad}");
            assert!(b.put(bad, b"x", None).await.is_err(), "{bad}");
        }
    }

    #[test]
    fn infer_content_type_by_extension() {
        assert_eq!(infer_content_type("a.png"), Some("image/png".into()));
        assert_eq!(infer_content_type("a.JPG"), Some("image/jpeg".into()));
        assert_eq!(infer_content_type("a.svg"), Some("image/svg+xml".into()));
        assert_eq!(infer_content_type("a.pdf"), Some("application/pdf".into()));
        assert_eq!(
            infer_content_type("a.json"),
            Some("application/json".into())
        );
        assert_eq!(infer_content_type("a.mp4"), Some("video/mp4".into()));
        assert_eq!(infer_content_type("a.unknown"), None);
    }

    /// 非 default local 后端 url() 明确报错（下载路由仅服务 default，spec §2 裁决）。
    #[tokio::test(flavor = "current_thread")]
    async fn local_blob_url_errors_when_not_default() {
        let root = tmp_root();
        let b = LocalBlob::named("img", &root, "/v1/api").unwrap();
        let e = b
            .url("k")
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            e.contains("only available for the 'default' backend"),
            "{e}"
        );
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
        reg.register(
            "default",
            Arc::new(LocalBlob::new(&root_d, "/v1/api").unwrap()),
        )
        .unwrap();
        reg.register("img", Arc::new(LocalBlob::new(&root_i, "/v1/api").unwrap()))
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                blobs: Some(Arc::new(reg)),
                ..Default::default()
            },
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
        assert!(
            v["data"]["err"]
                .as_str()
                .unwrap()
                .contains("blob backend 'ghost' not configured"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_serve_returns_bytes_and_content_type() {
        let root = tmp_root();
        let b = LocalBlob::new(&root, "/v1/api").unwrap();
        b.put("d/e.txt", b"hello", Some("text/plain"))
            .await
            .unwrap();
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

    /// ABI 10 流式上传（local）：chunk append → finish rename 转正 + ct sidecar；
    /// abort 删临时文件；未知 upload id 明确报错；非法 key 拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn local_put_stream_roundtrip_abort_and_cleanup() {
        let root = tmp_root();
        let b = LocalBlob::new(&root, "/v1/api").unwrap();

        // 非法 key：open 即拒
        assert!(b.put_stream_open("../evil", None).await.is_err());

        // 正常路径：三段 chunk → finish；显式 ct（与推断不同）落 sidecar
        let id = b
            .put_stream_open("up/big.bin", Some("application/x-custom"))
            .await
            .unwrap();
        b.put_stream_chunk(id, b"chunk1-").await.unwrap();
        b.put_stream_chunk(id, b"chunk2").await.unwrap();
        b.put_stream_finish(id).await.unwrap();
        assert_eq!(
            b.get("up/big.bin").await.unwrap(),
            b"chunk1-chunk2".to_vec()
        );
        assert_eq!(
            b.content_type("up/big.bin").await.unwrap().as_deref(),
            Some("application/x-custom")
        );
        // finish 后再 chunk → 未知 id
        assert!(b.put_stream_chunk(id, b"x").await.is_err());

        // abort 路径：临时文件被清理，最终对象不存在
        let id2 = b.put_stream_open("up/aborted.bin", None).await.unwrap();
        b.put_stream_chunk(id2, b"partial").await.unwrap();
        b.put_stream_abort(id2).await.unwrap();
        assert!(b.get("up/aborted.bin").await.is_err());
        // 临时文件名前缀 .oj-tmp-upload- 不残留
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".oj-tmp-upload")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "abort 后临时文件必须清理: {leftovers:?}"
        );

        // 未知 id：finish/abort 明确报错（幂等友好，文案点名）
        let e = b
            .put_stream_finish(999)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("unknown upload"), "{e}");

        // 无 ct 的流式：靠扩展名推断
        let id3 = b.put_stream_open("up/pic.png", None).await.unwrap();
        b.put_stream_chunk(id3, b"PNG").await.unwrap();
        b.put_stream_finish(id3).await.unwrap();
        assert_eq!(
            b.content_type("up/pic.png").await.unwrap().as_deref(),
            Some("image/png")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn op_blob_upload_url_local_backend_errors_with_direct_put_hint() {
        let root = tmp_root();
        let local = LocalBlob::new(&root, "/v1/api").unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                blobs: Some(registry_with_default(Arc::new(local))),
                ..Default::default()
            },
        );
        // 缺省 opts = {"kind":"put"}；local 后端无预签名 → 错误文案指路直传 PUT 路由。
        let cap = b
            .run_with(
                r#"
                (async () => {
                    try {
                        await blob.uploadUrl("a/big.bin");
                        json.ok({ err: "" });
                    } catch (e) {
                        json.ok({ err: String(e) });
                    }
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let err = v["data"]["err"].as_str().unwrap_or_default();
        assert!(
            err.contains("local blob backend has no upload presign")
                && err.contains("direct PUT upload route"),
            "{err}"
        );
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
            Extras {
                blobs: Some(registry_with_default(Arc::new(local))),
                ..Default::default()
            },
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
