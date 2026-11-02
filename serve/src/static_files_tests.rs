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
