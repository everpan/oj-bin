//! URL 形态 api mount 的装配期物化（v0.1.77）。
//!
//! 把 config 里的 mounts[].api = http(s)://... 拉取为「dist 根 tarball」并解压到
//! 内容寻址缓存（<cache>/<sha256>/），以 release 语义服务。完整性锚 = 声明 sha256；
//! 解压器逐条目 fail-fast，拒绝 tar-slip / 链接 / 特殊文件 / pax·gnu 元数据，并对解压
//! 总量 / 条目数 / 单条目大小设上限（防 zip / 稀疏炸弹）。
//!
//! 设计依据：docs/plans/2026-10-17-url-api-mount-design.md（含四路评审处置 §9）。

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;

/// 下载上限（单文件）。
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// 解压后累计字节上限（防 zip 炸弹）。
const MAX_UNCOMPRESSED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// 条目数上限。
const MAX_ENTRIES: usize = 100_000;
/// 单条目逻辑大小上限。
const MAX_ENTRY_BYTES: u64 = 1 * 1024 * 1024 * 1024;

/// per-sha256 进程内互斥，串行化同 URL 的物化（消并发 rename 竞态 / 崩溃窗口死锁）。
static SHA_LOCKS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, std::sync::Arc<Mutex<()>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// 取（或创建）该 sha256 的专用锁。
fn sha_lock(sha: &str) -> std::sync::Arc<Mutex<()>> {
    let mut g = SHA_LOCKS.lock().unwrap();
    g.entry(sha.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

/// 单例 reqwest Client：显式 connect/read 超时 + 受限重定向策略（安全 B1/m6）。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("mount_fetch: reqwest client build")
    })
}

/// 物化一个 URL api mount：返回解压后的 dist 根目录（含 manifests.yaml）。
///
/// - url：http(s) 远程 tarball。
/// - sha256：64 hex 完整性锚。
/// - cache_root：内容寻址缓存根（相对 config_dir，调用方已绝对化）。
pub fn materialize(url: &str, sha256: &str, cache_root: &Path) -> Result<PathBuf, String> {
    let sha = sha256.to_ascii_lowercase();
    let target = cache_root.join(&sha);

    // 串行化：同 sha 同时只一个写者。
    let lock = sha_lock(&sha);
    let _guard = lock.lock().unwrap();

    // 命中：缓存目录已完整（.complete 存在）。轻量复验（安全 M1）：重算已存 .tgz。
    if target.join(".complete").is_file() {
        if reverify(&target, &sha).is_ok() {
            return Ok(target);
        }
        // 缓存被篡改 / 损坏 → 清掉重拉，而非静默用坏数据。
        let _ = std::fs::remove_dir_all(&target);
    }
    // 进入物化分支：若 target 存在但无 .complete（上轮崩溃残留 / 陈旧半成品），先清。
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }

    // 下载 + 解压在专用线程 + 自建 runtime 上跑（不阻塞调用方 tokio runtime）。
    let url = url.to_string();
    let cache_root = cache_root.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<Result<PathBuf, String>>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let res = match rt {
            Ok(rt) => rt.block_on(async move { do_fetch(&url, &sha, &cache_root).await }),
            Err(e) => Err(format!("mount fetch runtime: {e}")),
        };
        let _ = tx.send(res);
    });
    rx.recv()
        .map_err(|e| format!("mounts: fetch thread died: {e}"))?
}

/// 命中复验：重算 target/.artifact.tgz 的 sha256 与声明比对。
fn reverify(target: &Path, sha: &str) -> Result<(), String> {
    let tgz = target.join(".artifact.tgz");
    let data = std::fs::read(&tgz).map_err(|e| format!("cache reverify read: {e}"))?;
    let got = hex(&Sha256::digest(&data));
    if got != sha {
        return Err(format!("cache sha256 mismatch: declared {sha}, got {got}"));
    }
    Ok(())
}

async fn do_fetch(url: &str, sha: &str, cache_root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(cache_root)
        .map_err(|e| format!("mounts: create cache dir {}: {e}", cache_root.display()))?;

    let target = cache_root.join(sha);
    let pid = std::process::id();

    // 1) 下载到同文件系统临时文件（create_new 防预创建 symlink）。
    let tmp = cache_root.join(format!(".tmp-{sha}-{pid}-0"));
    let _ = std::fs::remove_file(&tmp);
    let mut tmpf = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("mounts: create temp {}: {e}", tmp.display()))?;

    let resp = http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("mounts: fetch {url} failed: {e}"))?;
    if !resp.status().is_success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("mounts: fetch {url} bad status {status}", status = resp.status()));
    }
    let mut stream = resp.bytes_stream();
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("mounts: fetch {url} stream: {e}")
        })?;
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD_BYTES {
            let _ = std::fs::remove_file(&tmp);
            return Err("mounts: download exceeds 512 MiB cap".into());
        }
        hasher.update(&chunk);
        tmpf.write_all(&chunk)
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("mounts: write temp: {e}")
            })?;
    }
    tmpf.flush().map_err(|e| format!("mounts: flush temp: {e}"))?;
    drop(tmpf);

    let got = hex(&hasher.finalize());
    if got != sha {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "mounts: sha256 mismatch for {url}: declared {sha}, got {got} (artifact changed or tampered; update config or investigate the CDN)"
        ));
    }

    // 2) 解压到 staging 目录（手写逐 entry 循环，不用 Archive::unpack）。
    let stage = cache_root.join(format!(".stage-{sha}-{pid}-0"));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .map_err(|e| format!("mounts: create stage {}: {e}", stage.display()))?;
    extract_tarball(&tmp, &stage)?;

    // 3) 产物契约：staging 根必须含 manifests.yaml。
    if !stage.join("manifests.yaml").is_file() {
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_file(&tmp);
        return Err(
            "mounts: archive root must contain manifests.yaml (pack an oj build dist tree as .tgz)"
                .into(),
        );
    }

    // 4) 把 .tgz 留作复验用，写 .complete 标记（rename 前预置，缩短崩溃窗口）。
    let _ = std::fs::copy(&tmp, stage.join(".artifact.tgz"));
    std::fs::write(stage.join(".complete"), b"")
        .map_err(|e| format!("mounts: write .complete: {e}"))?;

    // 5) O_EXCL 占位 target，再 rename staging → target。
    let placeholder = cache_root.join(format!(".ph-{sha}-{pid}-0"));
    let _ = std::fs::remove_file(&placeholder);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&placeholder)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if target.join(".complete").is_file() {
                let _ = std::fs::remove_file(&placeholder);
                let _ = std::fs::remove_dir_all(&stage);
                return Ok(target);
            }
            return Err(format!("mounts: target {target} already exists", target = target.display()));
        }
        Err(e) => return Err(format!("mounts: create target placeholder: {e}")),
    }
    let _ = std::fs::remove_file(&placeholder);

    match std::fs::rename(&stage, &target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if target.join(".complete").is_file() {
                return Ok(target);
            }
            return Err(format!("mounts: target {target} already exists", target = target.display()));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&target);
            return Err(format!(
                "mounts: rename stage to {target}: {e}",
                target = target.display()
            ));
        }
    }
    Ok(target)
}

/// 手写逐 entry 解压，带 tar-slip / 链接 / 特殊文件 / pax·gnu / 大小预算 防护。
fn extract_tarball(tgz: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(tgz)
        .map_err(|e| format!("mounts: open {tgz}: {e}", tgz = tgz.display()))?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .map_err(|e| format!("mounts: read tar entries: {e}"))?;

    let mut count: usize = 0;
    let mut total: u64 = 0;
    let bs = char::from(0x5C); // 反斜杠，专用于 Windows 路径形态检查

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("mounts: tar entry: {e}"))?;
        let header = entry.header();
        let etype = header.entry_type();
        let name = match entry.path() {
            Ok(p) => p,
            Err(e) => return Err(format!("mounts: entry path: {e}")),
        };
        let name_lossy = name.to_string_lossy().into_owned();

        // pax / gnu 元数据 / 稀疏 / 未知类型：SKIP（不写出、不拒绝）。
        match etype {
            EntryType::Regular | EntryType::Directory => {}
            EntryType::Symlink | EntryType::Link | EntryType::Char | EntryType::Block
            | EntryType::Fifo => {
                return Err(format!(
                    "mounts: archive entry {name_lossy} rejected: unsupported entry type"
                ));
            }
            _ => continue,
        }

        // 路径安全：拒绝反斜杠、绝对形态、.. / 根 / 前缀 组件。
        if name_lossy.contains(bs) {
            return Err(format!(
                "mounts: archive entry {name_lossy} rejected: backslash in path"
            ));
        }
        let mut rel = PathBuf::new();
        for c in name.components() {
            match c {
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    return Err(format!(
                        "mounts: archive entry {name_lossy} rejected: unsafe path component"
                    ));
                }
                Component::CurDir => {}
                Component::Normal(s) => rel.push(s),
            }
        }
        let out = dest.join(&rel);
        // lexical 兜底：落盘点必须仍在 dest 内。
        if !out.starts_with(dest) {
            return Err(format!(
                "mounts: archive entry {name_lossy} rejected: escaped destination"
            ));
        }

        // 大小预算（按逻辑 size 计，覆盖稀疏炸弹）。
        let size = header
            .size()
            .map_err(|e| format!("mounts: entry size: {e}"))?;
        total = total.saturating_add(size);
        if size > MAX_ENTRY_BYTES {
            return Err(format!(
                "mounts: archive entry {name_lossy} rejected: single entry exceeds 1 GiB"
            ));
        }
        if total > MAX_UNCOMPRESSED_BYTES {
            return Err("mounts: archive rejected: uncompressed total exceeds 4 GiB".into());
        }
        count += 1;
        if count > MAX_ENTRIES {
            return Err("mounts: archive rejected: too many entries (>100000)".into());
        }

        match etype {
            EntryType::Directory => {
                std::fs::create_dir_all(&out)
                    .map_err(|e| format!("mounts: mkdir {out}: {e}", out = out.display()))?;
            }
            EntryType::Regular => {
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!(
                            "mounts: mkdir parent {parent}: {e}",
                            parent = parent.display()
                        )
                    })?;
                }
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("mounts: read entry {name_lossy}: {e}"))?;
                std::fs::write(&out, &buf)
                    .map_err(|e| format!("mounts: write {out}: {e}", out = out.display()))?;
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

/// 字节转 hex 串。
fn hex(d: &[u8]) -> String {
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
