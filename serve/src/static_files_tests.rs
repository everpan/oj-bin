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
