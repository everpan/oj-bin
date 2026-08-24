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
use object_store::{ObjectStore, PutPayload};

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
}

impl LocalBlob {
    /// root 绝对/相对均可（调用方负责相对 config_dir 绝对化）；url 前缀 = {base}/blob。
    pub fn new(root: &std::path::Path, base_url: &str) -> BridgeResult<Self> {
        std::fs::create_dir_all(root).map_err(|e| format!("blob root {}: {e}", root.display()))?;
        Ok(Self {
            store: LocalFileSystem::new_with_prefix(root).map_err(|e| format!("blob root {}: {e}", root.display()))?,
            root: root.to_path_buf(),
            base_url: base_url.trim_end_matches('/').to_string(),
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

fn backend(state: &OpState) -> Result<Arc<dyn BlobBackend>, JsErrorBox> {
    state
        .borrow::<Arc<StableState>>()
        .blob
        .clone()
        .ok_or_else(|| JsErrorBox::generic("blob not configured (config blob: section missing)"))
}

/// blob.put(key, bytes, contentType?)。
#[op2]
pub async fn op_blob_put(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[buffer] bytes: JsBuffer,
    #[string] content_type: Option<String>,
) -> Result<bool, JsErrorBox> {
    let b = { backend(&state.borrow())? };
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
    #[string] key: String,
) -> Result<Vec<u8>, JsErrorBox> {
    let b = { backend(&state.borrow())? };
    b.get(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// blob.del(key)（幂等）。
#[op2]
pub async fn op_blob_del(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<bool, JsErrorBox> {
    let b = { backend(&state.borrow())? };
    b.del(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// blob.url(key) → 下载地址。
#[op2]
#[string]
pub async fn op_blob_url(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<String, JsErrorBox> {
    let b = { backend(&state.borrow())? };
    b.url(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// blob.contentType(key)。
#[op2]
#[string]
pub async fn op_blob_content_type(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<String>, JsErrorBox> {
    let b = { backend(&state.borrow())? };
    b.content_type(&key)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
