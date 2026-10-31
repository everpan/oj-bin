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

    /// 原始文件名（下载 Content-Disposition 用；U-18 导出产物）。默认 None——
    /// 后端不存元数据则不下发该头（s3 腿尽力口径的缺省形态）。
    async fn filename(&self, _key: &str) -> BridgeResult<Option<String>> {
        Ok(None)
    }
    /// 给在传的流式会话挂原始文件名（local finish 时落 `<key>.filename` sidecar；
    /// 不支持的后端 no-op——下载头照缺）。**已消毒**（无控制字符/引号/路径段）。
    async fn put_stream_filename(&self, _upload_id: u64, _filename: &str) -> BridgeResult<()> {
        Ok(())
    }
    /// 整段 put 路径挂原始文件名（回落路径用；语义同 put_stream_filename）。
    async fn set_filename(&self, _key: &str, _filename: &str) -> BridgeResult<()> {
        Ok(())
    }

    /// ---- ABI 11 服务端搬运 + 区间读 ----
    /// 复制（**src 保留**）。默认实现 `get` + `put`（字节进 V8）——能服务端复制的后端
    /// 必须覆盖，否则大文件搬运白付 2× 峰值。`src == dst` 为 no-op。
    async fn copy(&self, src: &str, dst: &str) -> BridgeResult<()> {
        if src == dst {
            return Ok(());
        }
        let bytes = self.get(src).await?;
        let ct = self.content_type(src).await?;
        self.put(dst, &bytes, ct.as_deref()).await
    }
    /// 搬移（**src 不再存在**）。默认实现 `copy` + `del`。`src == dst` 为 no-op。
    async fn move_to(&self, src: &str, dst: &str) -> BridgeResult<()> {
        if src == dst {
            return Ok(());
        }
        self.copy(src, dst).await?;
        self.del(src).await
    }
    /// 区间读（**短读截断**：`offset + len` 越过尾部只返回实际可读字节；`offset` 已过尾部
    /// 返回空）。默认实现 `get` 全量再切片——支持区间读的后端必须覆盖，否则嗅探也要读全文。
    async fn read_range(&self, key: &str, offset: u64, len: u64) -> BridgeResult<Vec<u8>> {
        let all = self.get(key).await?;
        let size = all.len() as u64;
        let start = offset.min(size) as usize;
        let end = offset.saturating_add(len).min(size) as usize;
        Ok(all[start..end].to_vec())
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
    /// 原始文件名（U-18 导出；finish 时落 `<key>.filename` sidecar → 下载 Content-Disposition）。
    filename: Option<String>,
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
    fn filename_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.filename"))
    }
    fn ct_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.ct"))
    }

    /// key → 文件系统绝对路径。**走 `LocalFileSystem` 自己的映射**（URL 段 ⇄ 路径），
    /// 不能手写 `root.join(key)`——key 含空格 / 非 ASCII 时两者的编码结果会分叉，
    /// copy/move 就打不到 `put`/`get` 写的那个文件上。
    fn fs_path(&self, key: &str) -> Result<PathBuf, String> {
        let p = os_path(key)?;
        self.store
            .path_to_filesystem(&p)
            .map_err(|e| format!("invalid blob key '{key}': {e}"))
    }

    /// 把 src 的 content_type 带给 dst（与 `put` 同语义：显式且与推断不同才写 sidecar，
    /// 否则清掉 dst 上的旧 sidecar——避免 dst 继承上一次的 ct）。
    fn carry_ct(&self, src: &str, dst: &str) -> Result<(), String> {
        let ct = std::fs::read_to_string(self.ct_path(src))
            .ok()
            .filter(|s| !s.is_empty());
        match ct
            .as_deref()
            .filter(|ct| infer_content_type(dst).as_deref() != Some(*ct))
        {
            Some(ct) => {
                let p = self.ct_path(dst);
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("blob ct dir: {e}"))?;
                }
                std::fs::write(p, ct).map_err(|e| format!("blob ct write: {e}"))?;
            }
            None => {
                let _ = std::fs::remove_file(self.ct_path(dst));
            }
        }
        Ok(())
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
    async fn filename(&self, key: &str) -> BridgeResult<Option<String>> {
        os_path(key)?;
        let p = self.filename_path(key);
        match std::fs::read_to_string(&p) {
            Ok(v) if !v.is_empty() => Ok(Some(v)),
            Ok(_) | Err(_) => Ok(None), // 无 sidecar = 无原名（尽力口径）
        }
    }

    async fn set_filename(&self, key: &str, filename: &str) -> BridgeResult<()> {
        os_path(key)?;
        let p = self.filename_path(key);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("blob filename dir: {e}"))?;
        }
        std::fs::write(p, filename).map_err(|e| format!("blob filename write: {e}"))?;
        Ok(())
    }

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
                filename: None,
                file: Some(file),
                finished: false,
            })),
        );
        Ok(id)
    }

    async fn put_stream_filename(&self, upload_id: u64, filename: &str) -> BridgeResult<()> {
        let s = {
            let m = self.uploads.lock().unwrap();
            m.get(&upload_id)
                .cloned()
                .ok_or_else(|| format!("blob put_stream_filename: unknown upload {upload_id}"))?
        };
        s.lock().await.filename = Some(filename.to_string());
        Ok(())
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
        // 原始文件名 sidecar（显式给才写；没有则清掉旧值，避免改名后继承）。
        match g.filename.take() {
            Some(name) => {
                let p = self.filename_path(&key);
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("blob filename dir: {e}"))?;
                }
                std::fs::write(p, name).map_err(|e| format!("blob filename write: {e}"))?;
            }
            None => {
                let _ = std::fs::remove_file(self.filename_path(&key));
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

    /// ABI 11 复制：**字节不进 V8**——`fs::copy`（内核 copy_file_range，页缓存级）。
    /// src 不存在即报错；`src == dst` 是 no-op（`fs::copy` 同文件语义未定义，会截断）。
    async fn copy(&self, src: &str, dst: &str) -> BridgeResult<()> {
        let s = self.fs_path(src)?;
        let d = self.fs_path(dst)?;
        if s == d {
            return Ok(());
        }
        if let Some(dir) = d.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("blob copy: {e}"))?;
        }
        std::fs::copy(&s, &d).map_err(|e| format!("blob copy: {e}"))?;
        self.carry_ct(src, dst)?;
        Ok(())
    }

    /// ABI 11 搬移：同父目录走 `fs::rename`（原子、零字节搬运）；跨目录 copy + unlink。
    /// src 不存在即报错（rename/copy 的 NotFound 直接透出）。
    async fn move_to(&self, src: &str, dst: &str) -> BridgeResult<()> {
        let s = self.fs_path(src)?;
        let d = self.fs_path(dst)?;
        if s == d {
            return Ok(());
        }
        if let Some(dir) = d.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("blob move: {e}"))?;
        }
        if s.parent() == d.parent() {
            std::fs::rename(&s, &d).map_err(|e| format!("blob move: {e}"))?;
        } else {
            std::fs::copy(&s, &d).map_err(|e| format!("blob move: {e}"))?;
            std::fs::remove_file(&s).map_err(|e| format!("blob move: {e}"))?;
        }
        self.carry_ct(src, dst)?;
        let _ = std::fs::remove_file(self.ct_path(src));
        Ok(())
    }

    /// ABI 11 区间读：seek + 定长读，**短读截断**（越尾只读到 EOF，offset 过尾返回空）。
    async fn read_range(&self, key: &str, offset: u64, len: u64) -> BridgeResult<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let p = self.fs_path(key)?;
        let mut f = tokio::fs::File::open(&p)
            .await
            .map_err(|e| format!("blob readRange: {e}"))?;
        let size = f
            .metadata()
            .await
            .map_err(|e| format!("blob readRange: {e}"))?
            .len();
        let start = offset.min(size);
        let end = offset.saturating_add(len).min(size);
        let n = end.saturating_sub(start) as usize;
        let mut buf = vec![0u8; n];
        if n > 0 {
            f.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| format!("blob readRange: {e}"))?;
            f.read_exact(&mut buf)
                .await
                .map_err(|e| format!("blob readRange: {e}"))?;
        }
        Ok(buf)
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

/// JS number（f64）→ u64 偏移/长度。NaN / 无穷 / 负数 / 非整数一律拒绝——数据截断比静默
/// 取整安全（offset 传 `1.5` 说明调用方算错了）。
fn num_to_u64(v: f64, what: &str) -> Result<u64, JsErrorBox> {
    if !v.is_finite() || v < 0.0 || v.fract() != 0.0 {
        return Err(JsErrorBox::generic(format!(
            "blob readRange: {what} must be a non-negative integer (got {v})"
        )));
    }
    Ok(v as u64)
}

/// blob.copy(src, dst)（ABI 11，v0.1.47）：**src 保留**。local = `fs::copy`，s3 = CopyObject；
/// 不支持服务端复制的后端回落 `get` + `put`（字节进 V8，仅作能力保险）。
#[op2]
pub async fn op_blob_copy(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] src: String,
    #[string] dst: String,
) -> Result<bool, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.copy(&src, &dst)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// blob.move(src, dst)（ABI 11，v0.1.47）：**src 不再存在**。local 同目录 = `fs::rename`
/// （原子、零字节），跨目录 = copy + unlink；s3 = CopyObject + DeleteObject。
#[op2]
pub async fn op_blob_move(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] src: String,
    #[string] dst: String,
) -> Result<bool, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    b.move_to(&src, &dst)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// blob.readRange(key, offset, len) → Uint8Array（ABI 11，v0.1.47）：**短读截断**——
/// `offset + len` 越尾只读到 EOF，offset 过尾返回空数组。后端不支持区间读时回落
/// `get` 全量再切片（local/s3 均原生支持，回落只是保险）。
#[op2]
#[buffer]
pub async fn op_blob_read_range(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
    offset: f64,
    len: f64,
) -> Result<Vec<u8>, JsErrorBox> {
    let b = { backend_named(&state.borrow(), &name)? };
    let (offset, len) = (num_to_u64(offset, "offset")?, num_to_u64(len, "len")?);
    b.read_range(&key, offset, len)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod tests;
