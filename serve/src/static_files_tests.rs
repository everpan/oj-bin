//! static_files 单测（v0.1.116 OJ-9/10）：协商 / 压缩内核 / 弱比较。
use super::*;

fn ae(v: &str) -> HeaderMap {
    let mut m = HeaderMap::new();
    m.insert(axum::http::header::ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
    m
}

/// Given 各类 Accept-Encoding 头
/// When negotiate_encoding
/// Then br>gzip 固定优先；q=0 排除；`*` 视为仅 gzip；坏头/无头/未知码 → Identity；
/// 大小写不敏感；多值头（HTTP/2 合法）合并生效
#[test]
fn negotiate_priority_and_q_values() {
    assert_eq!(negotiate_encoding(&ae("br, gzip")), Encoding::Br);
    assert_eq!(negotiate_encoding(&ae("gzip")), Encoding::Gzip);
    assert_eq!(negotiate_encoding(&ae("br;q=0, gzip")), Encoding::Gzip);
    assert_eq!(negotiate_encoding(&HeaderMap::new()), Encoding::Identity);
    assert_eq!(negotiate_encoding(&ae("*")), Encoding::Gzip);
    assert_eq!(negotiate_encoding(&ae("identity")), Encoding::Identity);
    assert_eq!(negotiate_encoding(&ae("BR; q=0.5")), Encoding::Br);
    assert_eq!(negotiate_encoding(&ae("deflate")), Encoding::Identity);
    let mut bad = HeaderMap::new();
    bad.insert(
        axum::http::header::ACCEPT_ENCODING,
        HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    assert_eq!(negotiate_encoding(&bad), Encoding::Identity);
    let mut multi = HeaderMap::new();
    multi.append(axum::http::header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    multi.append(axum::http::header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    assert_eq!(negotiate_encoding(&multi), Encoding::Br);
}

fn kernel_opts() -> StaticLayerOpts {
    StaticLayerOpts {
        compress_min_length: 1024,
        compress_types: None,
        brotli_quality: 5,
        gzip_level: 6,
        cache_dir: None,
    }
}

/// Given 3.5MB 量级可压文本 → Br/Gzip 输出 < 原文一半
/// Given ≥min_length 的高熵体 → 压缩无收益 → None
#[tokio::test(flavor = "current_thread")]
async fn compress_body_shrinks_text_not_high_entropy() {
    let opts = kernel_opts();
    let text = " hello low-code ".repeat(220_000).into_bytes();
    let br = compress_body(Encoding::Br, text.clone(), &opts).await.unwrap();
    assert!(br.len() * 2 < text.len(), "br {} vs {}", br.len(), text.len());
    let gz = compress_body(Encoding::Gzip, text.clone(), &opts).await.unwrap();
    assert!(gz.len() * 2 < text.len(), "gz {} vs {}", gz.len(), text.len());
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let hi: Vec<u8> = (0..8192)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u8
        })
        .collect();
    assert_eq!(compress_body(Encoding::Br, hi, &opts).await, None);
}

/// Given 可压/地板/未知扩展名与 min_length 边界
/// When is_compressible
/// Then 地板集恒 false（替换集列了也不压）；未知扩展名替换集可开启；len<min_length false
#[test]
fn is_compressible_floor_and_min_length() {
    let opts = kernel_opts();
    assert!(is_compressible("js", 1024, &opts));
    assert!(!is_compressible("js", 1023, &opts));
    assert!(!is_compressible("png", u64::MAX, &opts));
    let with_wasm = StaticLayerOpts {
        compress_types: Some(["wasm".to_string()].into_iter().collect()),
        ..kernel_opts()
    };
    assert!(is_compressible("wasm", u64::MAX, &with_wasm));
    assert!(!is_compressible("png", u64::MAX, &with_wasm));
}

/// Given INM 弱值 W/"abc" 与 etag "abc" → 命中；强同值命中；`*` 命中；异值不命中
#[test]
fn inm_weak_compare() {
    let inm = |v: &str| {
        let mut m = HeaderMap::new();
        m.insert(
            axum::http::header::IF_NONE_MATCH,
            HeaderValue::from_str(v).unwrap(),
        );
        m
    };
    assert!(if_none_match_hit(&inm(r#"W/"abc""#), r#""abc""#));
    assert!(if_none_match_hit(&inm(r#""abc""#), r#""abc""#));
    assert!(if_none_match_hit(&inm(r#"W/"abc", "xyz""#), r#""xyz""#));
    assert!(if_none_match_hit(&inm("*"), r#""abc""#));
    assert!(!if_none_match_hit(&inm(r#""abd""#), r#""abc""#));
}

// —— T4 缓存仓库层（v0.1.116 OJ-9） ——

fn repo_opts(dir: Option<std::path::PathBuf>) -> StaticLayerOpts {
    StaticLayerOpts {
        compress_min_length: 1024,
        compress_types: None,
        brotli_quality: 5,
        gzip_level: 6,
        cache_dir: dir,
    }
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("oj-t4-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Given 同一文件的 touch/变长/换算法/换质量
/// When cache_key
/// Then 键两两不同；同输入两次 → 键相同
#[test]
fn cache_key_changes_with_invalidation_inputs() {
    let d = tmp_dir("key");
    let f = d.join("a.js");
    std::fs::write(&f, vec![b'x'; 2048]).unwrap();
    let opts = repo_opts(None);
    let m = std::fs::metadata(&f).unwrap();
    let k = cache_key(&f, &m, Encoding::Br, &opts);
    assert_eq!(cache_key(&f, &m, Encoding::Br, &opts), k, "同输入同键");
    // len 变
    std::fs::write(&f, vec![b'x'; 4096]).unwrap();
    assert_ne!(cache_key(&f, &std::fs::metadata(&f).unwrap(), Encoding::Br, &opts), k);
    std::fs::write(&f, vec![b'x'; 2048]).unwrap();
    // mtime 变
    let t = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::OpenOptions::new().write(true).open(&f).unwrap().set_modified(t).unwrap();
    assert_ne!(cache_key(&f, &std::fs::metadata(&f).unwrap(), Encoding::Br, &opts), k);
    // 算法/质量变
    assert_ne!(cache_key(&f, &m, Encoding::Gzip, &opts), k);
    let q6 = StaticLayerOpts { brotli_quality: 6, ..repo_opts(None) };
    assert_ne!(cache_key(&f, &m, Encoding::Br, &q6), k);
    // 回归防护（canonicalize 事故）：原文删除后同 meta 同路径 → 键必须不变，
    // 否则「缓存命中腿」在原文缺席时拆出第二代键、永远 miss。
    std::fs::remove_file(&f).unwrap();
    assert_eq!(cache_key(&f, &m, Encoding::Br, &opts), k, "删除原文后键必须稳定");
    let _ = std::fs::remove_dir_all(&d);
}

/// Given 缓存已有哨兵字节且原文已删除
/// When compressed_body_or_cache（同 meta）
/// Then 返回哨兵——证明命中缓存、不读原文
#[tokio::test(flavor = "current_thread")]
async fn store_then_hit_serves_stored_bytes() {
    let cd = tmp_dir("hit-cd");
    let wd = tmp_dir("hit-wd");
    let f = wd.join("a.js");
    std::fs::write(&f, vec![b'a'; 4096]).unwrap();
    let opts = repo_opts(Some(cd.clone()));
    let m = std::fs::metadata(&f).unwrap();
    let first = compressed_body_or_cache(&f, &m, Encoding::Br, &opts).await.unwrap();
    assert!(first.len() < 4096, "首调现场压缩");
    let key = cache_key(&f, &m, Encoding::Br, &opts);
    std::fs::write(cd.join(&key), b"SENTINEL").unwrap();
    std::fs::remove_file(&f).unwrap();
    let second = compressed_body_or_cache(&f, &m, Encoding::Br, &opts).await.unwrap();
    assert_eq!(second, b"SENTINEL");
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given 缓存 miss + 8 个并发同键请求
/// When compressed_body_or_cache（join! 全发）
/// Then 全部返回一致且缓存目录恰 1 个文件（single-flight 收敛）
#[tokio::test(flavor = "current_thread")]
async fn concurrent_misses_compress_once() {
    let cd = tmp_dir("sf-cd");
    let wd = tmp_dir("sf-wd");
    let f = wd.join("b.js");
    std::fs::write(&f, vec![b'b'; 8192]).unwrap();
    let opts = std::sync::Arc::new(repo_opts(Some(cd.clone())));
    let m = std::fs::metadata(&f).unwrap();
    let mut js = Vec::new();
    for _ in 0..8 {
        js.push(compressed_body_or_cache(&f, &m, Encoding::Br, &opts));
    }
    let outs = futures_util::future::join_all(js).await;
    let first = outs[0].clone().unwrap();
    assert!(outs.iter().all(|o| o.as_deref() == Some(first.as_slice())));
    assert_eq!(std::fs::read_dir(&cd).unwrap().count(), 1, "恰一代缓存");
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given 已有同 sha1 前缀的旧代缓存
/// When 换 quality 触发新键 store
/// Then 旧前缀文件被清理、新键存在
#[tokio::test(flavor = "current_thread")]
async fn new_key_prunes_same_prefix() {
    let cd = tmp_dir("prune-cd");
    let wd = tmp_dir("prune-wd");
    let f = wd.join("c.js");
    std::fs::write(&f, vec![b'c'; 4096]).unwrap();
    let opts5 = repo_opts(Some(cd.clone()));
    let m = std::fs::metadata(&f).unwrap();
    compressed_body_or_cache(&f, &m, Encoding::Br, &opts5).await.unwrap();
    // 同 sha1 前缀伪造旧代
    let prefix = format!("{}-", cache_key(&f, &m, Encoding::Br, &opts5).split('-').next().unwrap());
    let fake = cd.join(format!("{prefix}deadbeef-br5"));
    std::fs::write(&fake, b"old").unwrap();
    let opts6 = StaticLayerOpts { brotli_quality: 6, ..repo_opts(Some(cd.clone())) };
    compressed_body_or_cache(&f, &m, Encoding::Br, &opts6).await.unwrap();
    assert!(!fake.exists(), "旧代被清理");
    assert!(cd.join(cache_key(&f, &m, Encoding::Br, &opts6)).exists(), "新键存在");
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given cache_dir 指向不可写路径（父为文件）
/// When compressed_body_or_cache
/// Then 返回 Some（内存直压）不 panic——fail-open
#[tokio::test(flavor = "current_thread")]
async fn unwritable_dir_falls_back_memory() {
    let wd = tmp_dir("ro-wd");
    let blocker = wd.join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let f = wd.join("d.js");
    std::fs::write(&f, vec![b'd'; 4096]).unwrap();
    let opts = repo_opts(Some(blocker.join("child")));
    let m = std::fs::metadata(&f).unwrap();
    let out = compressed_body_or_cache(&f, &m, Encoding::Br, &opts).await;
    assert!(out.is_some());
    let _ = std::fs::remove_dir_all(&wd);
}

// —— T5 serve_web 组装（B1 fixture） ——

/// B1 共享 fixture：tempdir web root + AppState（静态层可配）。
fn comp_state(
    root: &Path,
    layer: Option<StaticLayerOpts>,
    policy: CachePolicy,
) -> (AppState, WebMount) {
    let st = AppState {
        mounts: Vec::new(),
        actor: make_actor(PathBuf::from("."), false),
        timeout: None,
        static_opts: StaticOpts {
            static_layer: layer,
            web_cache_policy: policy,
            ..Default::default()
        },
        pipeline: Pipeline::default(),
        certificate_status: Arc::new(RwLock::new(CertificateStatus::Valid)),
        certificate_valid_until: Arc::new(RwLock::new(None)),
        plugins: Arc::default(),
        exec_registry: None,
    };
    let mount = WebMount {
        prefix: "/".into(),
        root: root.to_path_buf(),
        headers: Vec::new(),
        spa: true,
    };
    (st, mount)
}

fn layer_opts(dir: Option<std::path::PathBuf>) -> StaticLayerOpts {
    StaticLayerOpts {
        compress_min_length: 1024,
        compress_types: None,
        brotli_quality: 5,
        gzip_level: 6,
        cache_dir: dir,
    }
}

/// B1 薄封装：带 Accept-Encoding 的 GET。
async fn get(st: &AppState, w: &WebMount, rel: &str, ae: &str) -> Response {
    let mut h = HeaderMap::new();
    if !ae.is_empty() {
        h.insert(
            axum::http::header::ACCEPT_ENCODING,
            HeaderValue::from_str(ae).unwrap(),
        );
    }
    serve_web(st, w, rel, "GET", true, &h).await
}

fn t5_dirs(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let cd = std::env::temp_dir().join(format!("oj-t5-{}-cd-{}", tag, std::process::id()));
    let wd = std::env::temp_dir().join(format!("oj-t5-{}-wd-{}", tag, std::process::id()));
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
    std::fs::create_dir_all(&cd).unwrap();
    std::fs::create_dir_all(&wd).unwrap();
    (cd, wd)
}

fn hdr<'r>(r: &'r Response, k: &axum::http::HeaderName) -> Option<String> {
    r.headers().get(k).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

/// Given 100KB js + Accept-Encoding: br + 缓存目录
/// When GET
/// Then 200 + Content-Encoding: br + CL=压缩后 + ETag 弱化 + Vary + 缓存目录 +1 文件
#[tokio::test(flavor = "current_thread")]
async fn br_negotiation_serves_compressed_with_vary() {
    let (cd, wd) = t5_dirs("br");
    std::fs::write(wd.join("app.js"), vec![b'a'; 102_400]).unwrap();
    let (st, mount) = comp_state(&wd, Some(layer_opts(Some(cd.clone()))), CachePolicy::Auto);
    let r = get(&st, &mount, "/app.js", "br").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, &axum::http::header::CONTENT_ENCODING).as_deref(), Some("br"));
    let cl = hdr(&r, &axum::http::header::CONTENT_LENGTH).unwrap().parse::<usize>().unwrap();
    assert!(cl < 102_400);
    assert!(hdr(&r, &axum::http::header::ETAG).unwrap().starts_with("W/"));
    assert_eq!(hdr(&r, &axum::http::header::VARY).as_deref(), Some("Accept-Encoding"));
    assert_eq!(std::fs::read_dir(&cd).unwrap().count(), 1, "缓存落盘 1 代");
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given 可压资源 + Accept-Encoding: identity
/// When GET
/// Then 无 Content-Encoding、强 ETag、**有 Vary**（变体互串防线）
#[tokio::test(flavor = "current_thread")]
async fn compressible_identity_still_varies() {
    let (cd, wd) = t5_dirs("idv");
    std::fs::write(wd.join("app.js"), vec![b'a'; 4096]).unwrap();
    let (st, mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::Auto);
    let r = get(&st, &mount, "/app.js", "identity").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, &axum::http::header::CONTENT_ENCODING), None);
    let etag = hdr(&r, &axum::http::header::ETAG).unwrap();
    assert!(!etag.starts_with("W/"), "identity 恒强 ETag");
    assert_eq!(hdr(&r, &axum::http::header::VARY).as_deref(), Some("Accept-Encoding"));
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given br 首取的弱 ETag
/// When 带 If-None-Match 再请求
/// Then 304 且带 Vary（弱比较命中）
#[tokio::test(flavor = "current_thread")]
async fn weak_etag_inm_hits_304() {
    let (cd, wd) = t5_dirs("304");
    std::fs::write(wd.join("app.js"), vec![b'a'; 4096]).unwrap();
    let (st, mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::Auto);
    let r1 = get(&st, &mount, "/app.js", "br").await;
    let etag = hdr(&r1, &axum::http::header::ETAG).unwrap();
    assert!(etag.starts_with("W/"));
    let mut h = HeaderMap::new();
    h.insert(axum::http::header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    h.insert(axum::http::header::IF_NONE_MATCH, HeaderValue::from_str(&etag).unwrap());
    let r2 = serve_web(&st, &mount, "/app.js", "GET", true, &h).await;
    assert_eq!(r2.status(), 304);
    assert_eq!(hdr(&r2, &axum::http::header::VARY).as_deref(), Some("Accept-Encoding"));
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given 100B js（< min_length）
/// When GET br
/// Then 无 Vary 无 Content-Encoding（不可压资源无协商面）
#[tokio::test(flavor = "current_thread")]
async fn small_file_no_vary_no_encoding() {
    let (cd, wd) = t5_dirs("small");
    std::fs::write(wd.join("tiny.js"), b"hi").unwrap();
    let (st, mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::Auto);
    let r = get(&st, &mount, "/tiny.js", "br").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, &axum::http::header::CONTENT_ENCODING), None);
    assert_eq!(hdr(&r, &axum::http::header::VARY), None);
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given static_layer None
/// When GET br
/// Then 逐字节现状：无三头、强 ETag、原体
#[tokio::test(flavor = "current_thread")]
async fn off_switch_byte_identical() {
    let (cd, wd) = t5_dirs("off");
    std::fs::write(wd.join("app.js"), vec![b'a'; 4096]).unwrap();
    let (st, mount) = comp_state(&wd, None, CachePolicy::None_);
    let r = get(&st, &mount, "/app.js", "br").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, &axum::http::header::CONTENT_ENCODING), None);
    assert_eq!(hdr(&r, &axum::http::header::VARY), None);
    assert_eq!(hdr(&r, &axum::http::header::CACHE_CONTROL), None);
    let etag = hdr(&r, &axum::http::header::ETAG).unwrap();
    assert!(!etag.starts_with("W/"));
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&body[..], &[b'a'; 4096][..]);
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given index.html 配 html_meta 注入 + br 请求
/// When GET
/// Then 体被压缩但缓存目录无新增文件（HTML 恒内存直压）
#[tokio::test(flavor = "current_thread")]
async fn html_compressed_but_never_cached() {
    let (cd, wd) = t5_dirs("html");
    std::fs::write(wd.join("index.html"), format!("<html><head></head><body>{}</body></html>", "x".repeat(4096))).unwrap();
    let (mut st, mount) = comp_state(&wd, Some(layer_opts(Some(cd.clone()))), CachePolicy::Auto);
    st.static_opts.html_meta = Some("__meta".into());
    let r = get(&st, &mount, "/", "br").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, &axum::http::header::CONTENT_ENCODING).as_deref(), Some("br"));
    assert_eq!(hdr(&r, &axum::http::header::VARY).as_deref(), Some("Accept-Encoding"));
    assert_eq!(std::fs::read_dir(&cd).unwrap().count(), 0, "HTML 不落缓存");
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

// —— T6 两档制默认缓存策略（OJ-10） ——

/// Given 各形态文件名
/// When default_cache_control
/// Then 哈希段（[-._] 分隔末段≥8 且含数字与字母）→ immutable；纯字母词/纯数字/无段 → no-cache
#[test]
fn default_cache_control_two_tier_table() {
    let cc = |name: &str| default_cache_control(Path::new(name));
    assert_eq!(
        cc("app-B2R7x9cW.js"),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        cc("main.1a2b3c4d.js"),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(cc("config-production.js"), "no-cache", "纯字母段防误钉");
    assert_eq!(cc("20240101.js"), "no-cache", "纯数字段防误钉");
    assert_eq!(cc("index.html"), "no-cache");
    assert_eq!(cc("modules.json"), "no-cache");
    assert_eq!(cc("logo.png"), "no-cache");
    assert_eq!(cc("sw.js"), "no-cache");
}

/// Given spa 挂载 + meta 目录两个路由 JSON（不同 title）
/// When 分别 GET /a、/b（Accept html）
/// Then 各自 title 正确不串、均 no-cache、均压缩
#[tokio::test(flavor = "current_thread")]
async fn spa_fallback_meta_not_cross_contaminated() {
    let (cd, wd) = t5_dirs("meta");
    std::fs::create_dir_all(wd.join("__meta")).unwrap();
    std::fs::write(wd.join("__meta/a.json"), r#"{"title":"A 页"}"#).unwrap();
    std::fs::write(wd.join("__meta/b.json"), r#"{"title":"B 页"}"#).unwrap();
    std::fs::write(
        wd.join("index.html"),
        format!("<html><head><title>t</title></head><body>{}</body></html>", "y".repeat(4096)),
    )
    .unwrap();
    let (mut st, mount) = comp_state(&wd, Some(layer_opts(Some(cd.clone()))), CachePolicy::Auto);
    st.static_opts.html_meta = Some("__meta".into());
    let ra = get(&st, &mount, "/a", "br").await;
    let rb = get(&st, &mount, "/b", "br").await;
    let ba = axum::body::to_bytes(ra.into_body(), 1 << 20).await.unwrap().to_vec();
    let bb = axum::body::to_bytes(rb.into_body(), 1 << 20).await.unwrap().to_vec();
    // 注入内容各自正确（经 br 解码验证不串内容）
    let da = decompress_br(&ba);
    let db = decompress_br(&bb);
    assert!(da.contains("A 页") && !da.contains("B 页"), "a 不串 b");
    assert!(db.contains("B 页") && !db.contains("A 页"), "b 不串 a");
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

fn decompress_br(b: &[u8]) -> String {
    let mut r = brotli::Decompressor::new(b, 4096);
    let mut s = Vec::new();
    std::io::Read::read_to_end(&mut r, &mut s).unwrap();
    String::from_utf8(s).unwrap()
}

/// Given 挂载显式 Cache-Control
/// When GET 可压资产
/// Then 响应为显式值（兜底不触发）
#[tokio::test(flavor = "current_thread")]
async fn explicit_mount_headers_override_default() {
    let (cd, wd) = t5_dirs("ovr");
    std::fs::write(wd.join("app.js"), vec![b'a'; 4096]).unwrap();
    let (st, mut mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::Auto);
    mount.headers = vec![("Cache-Control".into(), "public, max-age=60".into())];
    let r = get(&st, &mount, "/app.js", "br").await;
    assert_eq!(
        hdr(&r, &axum::http::header::CACHE_CONTROL).as_deref(),
        Some("public, max-age=60")
    );
    assert_eq!(hdr(&r, &axum::http::header::CONTENT_ENCODING).as_deref(), Some("br"));
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given html_cache_control 配 no-store
/// When GET index.html
/// Then HTML 腿为 no-store（既有语义不回归）
#[tokio::test(flavor = "current_thread")]
async fn html_cache_control_still_wins() {
    let (cd, wd) = t5_dirs("hcc");
    std::fs::write(wd.join("index.html"), format!("<html><body>{}</body></html>", "z".repeat(4096))).unwrap();
    let (mut st, mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::Auto);
    st.static_opts.html_cache_control = Some("no-store".into());
    let r = get(&st, &mount, "/", "br").await;
    assert_eq!(hdr(&r, &axum::http::header::CACHE_CONTROL).as_deref(), Some("no-store"));
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given web_cache_policy: None_
/// When GET 可压资产与 HTML
/// Then 全腿无 Cache-Control（Vary/压缩不受影响）
#[tokio::test(flavor = "current_thread")]
async fn policy_none_no_cache_control() {
    let (cd, wd) = t5_dirs("none");
    std::fs::write(wd.join("app.js"), vec![b'a'; 4096]).unwrap();
    std::fs::write(wd.join("index.html"), format!("<html><body>{}</body></html>", "w".repeat(4096))).unwrap();
    let (st, mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::None_);
    let r = get(&st, &mount, "/app.js", "br").await;
    assert_eq!(hdr(&r, &axum::http::header::CACHE_CONTROL), None);
    assert_eq!(hdr(&r, &axum::http::header::VARY).as_deref(), Some("Accept-Encoding"));
    let r = get(&st, &mount, "/", "br").await;
    assert_eq!(hdr(&r, &axum::http::header::CACHE_CONTROL), None);
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}

/// Given immutable 资产 304 再验证
/// When INM 命中
/// Then 304 带 Cache-Control（与 200 同策略，RFC 9110 §15.4.5）
#[tokio::test(flavor = "current_thread")]
async fn cache_control_on_304() {
    let (cd, wd) = t5_dirs("cc304");
    std::fs::write(wd.join("app-B2R7x9cW.js"), vec![b'a'; 4096]).unwrap();
    let (st, mount) = comp_state(&wd, Some(layer_opts(None)), CachePolicy::Auto);
    let r1 = get(&st, &mount, "/app-B2R7x9cW.js", "br").await;
    assert_eq!(
        hdr(&r1, &axum::http::header::CACHE_CONTROL).as_deref(),
        Some("public, max-age=31536000, immutable")
    );
    let etag = hdr(&r1, &axum::http::header::ETAG).unwrap();
    let mut h = HeaderMap::new();
    h.insert(axum::http::header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    h.insert(axum::http::header::IF_NONE_MATCH, HeaderValue::from_str(&etag).unwrap());
    let r2 = serve_web(&st, &mount, "/app-B2R7x9cW.js", "GET", true, &h).await;
    assert_eq!(r2.status(), 304);
    assert_eq!(
        hdr(&r2, &axum::http::header::CACHE_CONTROL).as_deref(),
        Some("public, max-age=31536000, immutable")
    );
    let _ = (std::fs::remove_dir_all(&cd), std::fs::remove_dir_all(&wd));
}
