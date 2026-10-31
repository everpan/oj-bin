//! 静态资源层：web 静态服务 + etag/range + blob key 解析。
use super::*;

pub(crate) async fn serve_web(
    st: &AppState,
    w: &WebMount,
    rel_path: &str,
    verb: &str,
    accept_html: bool,
    headers: &HeaderMap,
) -> Response {
    if !matches!(verb, "GET" | "HEAD") {
        return fail_response(
            405,
            &format!("method {verb} not allowed (web mounts serve GET/HEAD only)"),
        );
    }
    let meta = st.static_opts.html_meta.as_deref();
    if let Some(file) = resolve_static(&w.root, rel_path, meta) {
        let is_html = file
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("html"));
        // 条件请求（perf F9，非 HTML 资产）：ETag(mtime-len) 命中 If-None-Match →
        // 304 免读盘免传输。HTML 不做——体经 meta 注入后与文件不再逐字节一致，
        // 且缓存策略本就归 `html_cache_control` 管。
        let etag = if is_html {
            None
        } else {
            tokio::fs::metadata(&file)
                .await
                .ok()
                .and_then(|m| etag_of(&m))
        };
        if let Some(etag) = &etag
            && if_none_match_hit(headers, etag)
        {
            let mut r = Response::new(axum::body::Body::empty());
            *r.status_mut() = StatusCode::NOT_MODIFIED;
            if let Ok(v) = axum::http::HeaderValue::from_str(etag) {
                r.headers_mut().insert(axum::http::header::ETAG, v);
            }
            return r;
        }
        if let Ok(body) = tokio::fs::read(&file).await {
            let mut r = static_page(st, &w.root, rel_path, &file, body).await;
            if let Some(etag) = &etag
                && let Ok(v) = axum::http::HeaderValue::from_str(etag)
            {
                r.headers_mut().insert(axum::http::header::ETAG, v);
            }
            apply_custom_headers(&mut r, &site_headers(st, w));
            return r;
        }
    }
    // SPA 深链接回落（挂载 `spa: true`，v0.1.20 语义）：未命中 + 无扩展名 +
    // Accept html → 送 **本挂载** root/index.html。
    if w.spa && accept_html && !has_extension(rel_path) {
        let idx = w.root.join("index.html");
        if let Ok(body) = tokio::fs::read(&idx).await {
            let mut r = static_page(st, &w.root, rel_path, &idx, body).await;
            apply_custom_headers(&mut r, &site_headers(st, w));
            return r;
        }
    }
    fail_response(404, "no route matched")
}

/// 路由命中 → 前置管线（鉴权/租户/上传上限/multipart）→ 执行 handler → Capture 回写。
///
/// 自由函数而非 `handle()` 内的闭包：`handle()` 的静态兜底（v0.1.25 起还会内部派发
/// meta handler）与 `strip_app_prefix` 同款理由——闭包会**部分移动** `st`/`headers`，
/// 之后就没法再借用它们。
#[allow(clippy::too_many_arguments)]
pub(crate) fn wants_html(headers: &HeaderMap) -> bool {
    match headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
    {
        None => true,
        Some(v) => v
            .split(',')
            .map(|p| p.split(';').next().unwrap_or("").trim())
            .any(|t| t == "text/html" || t == "*/*"),
    }
}

/// 路径是否带扩展名（带扩展名 = 资源请求，不该回落成 HTML）。
pub(crate) fn has_extension(rel_path: &str) -> bool {
    rel_path
        .rsplit('/')
        .next()
        .and_then(|last| (last.contains('.')).then_some(true))
        .unwrap_or(false)
}

/// 静态响应（v0.1.20 起；v0.1.25 增动态 meta + HTML 缓存头）：HTML 走 per-route meta
/// 注入（静态 JSON 打底 → 动态 handler 覆盖），其余原样；什么都不配时逐字节同旧行为。
///
/// `rel_path` 是**已剥站点前缀**的请求路径（SPA 回落时即深链接本身，不是 index.html
/// 的落盘路径）——按它查 meta 才能做到「按路由」；`site_root` 是命中站点的磁盘根
/// （meta JSON 目录按站点各自解析，v0.1.27 多站点）。
pub(crate) async fn static_page(
    st: &AppState,
    site_root: &Path,
    rel_path: &str,
    file: &Path,
    body: Vec<u8>,
) -> Response {
    let is_html = file
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("html"));
    if !is_html {
        // 缓存头只管 HTML：js/css/图片仍交前置反代（见已知限制）。
        return file_response(file, body);
    }
    let opts = &st.static_opts;
    let (has_static, has_dynamic) = (opts.html_meta.is_some(), opts.html_meta_handler.is_some());
    if !has_static && !has_dynamic {
        // 注入面全关：**只**加缓存头（`html_cache_control` 是独立键——只想给壳挂
        // `no-cache` 而不做注入，是最常见的用法之一）；该键也没配才逐字节同旧行为。
        let mut r = file_response(file, body);
        if let Some(cc) = opts.html_cache_control.as_deref() {
            set_cache_control(&mut r, cc);
        }
        return r;
    }
    // 1) 静态 JSON 打底（`<site_root>/<dir>/<path>.json`，v0.1.20），2) 动态 handler 按 key 覆盖。
    let mut map = load_static_meta(site_root, opts.html_meta.as_deref(), rel_path).await;
    let Ok(text) = String::from_utf8(body) else {
        // 非 UTF-8 的「HTML」不是我们能注入的文档（保持 v0.1.20 的旧行为：空体）。
        return file_response(file, Vec::new());
    };
    let mut dynamic_cc: Option<String> = None;
    if has_dynamic {
        match dispatch_meta_handler(st, rel_path).await {
            Ok((m, cc)) => {
                dynamic_cc = cc;
                map.extend(m);
            }
            // fail-open：注入面坏了不能让页面跟着坏（爬虫/用户看到的仍是一份完整 HTML）。
            Err(e) => eprintln!("warn: html_meta_handler: {e}"),
        }
    }
    let out = inject_head(text, &map);
    let mut r = file_response(file, out.into_bytes());
    // 缓存头：动态 handler 的 cache_control > server.html_cache_control；都没有 = 不加。
    if let Some(cc) = dynamic_cc.as_deref().or(opts.html_cache_control.as_deref()) {
        set_cache_control(&mut r, cc);
    }
    r
}

/// blob 下载 key：percent-decode 每段后过 valid_key（防 `%2e%2e` 穿越，与 resolve_static 同款守卫）。
pub(crate) fn decode_blob_key(s: &str) -> Option<String> {
    let decoded = s
        .split('/')
        .map(|seg| percent_encoding::percent_decode_str(seg).decode_utf8().ok())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    only_js::bridge::valid_key(&decoded).then_some(decoded)
}

/// 静态文件解析：uri.path()（仍 percent-encoded）逐段解码后拼 root；
/// 根/目录 → index.html；越界段（`.`/`..`/`\`/`/`/`\0`/空段，含解码后——
/// `%2F` 走私等价穿越）→ None（404）。
/// 请求路径 → 命中站点的相对路径（剥站点 `prefix`，多站点最长命中由调用方保证）。
/// 前缀 "/" → 原样（全路径兜底，行为与旧版一致）；非 "/" 前缀：精确命中前缀根
/// → "/"（resolve_static 落 index.html），`前缀/...` → "/..."，前缀外（含仅前缀
/// 更长串如 `/sitex`）→ None（404）。
/// 自由函数（handle() 处 `st` 已被闭包部分移动，方法接收者会整借 `st`）。
pub(crate) fn strip_app_prefix<'a>(prefix: &str, path: &'a str) -> Option<&'a str> {
    if prefix == "/" {
        return Some(path);
    }
    if path == prefix {
        return Some("/");
    }
    path.strip_prefix(prefix)
        .filter(|rest| rest.starts_with('/'))
}

/// `meta_dir`：`server.html_meta` 的目录名——该目录是**数据**不是站点资产，
/// 命中即 404（v0.1.20）。
pub(crate) fn resolve_static(
    root: &Path,
    uri_path: &str,
    meta_dir: Option<&str>,
) -> Option<PathBuf> {
    let rel = uri_path.strip_prefix('/')?.trim_end_matches('/');
    let mut p = root.to_path_buf();
    if !rel.is_empty() {
        for seg in rel.split('/') {
            let s = percent_encoding::percent_decode_str(seg)
                .decode_utf8()
                .ok()?;
            if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0']) {
                return None;
            }
            if meta_dir.is_some_and(|d| d == s.as_ref()) {
                return None;
            }
            p.push(s.as_ref());
        }
    }
    if p.is_dir() {
        p.push("index.html");
    }
    p.is_file().then_some(p)
}

/// 扩展名 → Content-Type（常见集，未知 = octet-stream——ponytail: 嫌少再加）。
pub(crate) fn mime_of(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// blob 快速路径的 key 提取（perf F12）：免每请求 `format!("{prefix}/blob/")` 分配。
/// 与旧写法逐字节同语义——含 base `"/"` 的历史怪癖（`{prefix}/blob/` 拼出
/// `"//blob/"`，只有 `//blob/x` 形态命中；行为保留不改，修它属另一议题）。
pub(crate) fn blob_key_of<'a>(prefix: &str, path: &'a str) -> Option<&'a str> {
    if prefix == "/" {
        path.strip_prefix("//blob/")
    } else {
        path.strip_prefix(prefix)
            .and_then(|r| r.strip_prefix("/blob/"))
    }
}

/// 静态资产 ETag（perf F9）：强 ETag `"mtime-hex len-hex"`——体即文件字节，精确成立。
pub(crate) fn etag_of(m: &std::fs::Metadata) -> Option<String> {
    let len = m.len();
    let mtime = m.modified().ok()?;
    let nanos = mtime
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!(r#""{nanos:x}-{len:x}""#))
}

/// If-None-Match 命中判定（弱比较，务实口径）：`*` 或值列表包含本 ETag 即命中。
pub(crate) fn if_none_match_hit(headers: &HeaderMap, etag: &str) -> bool {
    match headers.get(axum::http::header::IF_NONE_MATCH) {
        Some(v) => match v.to_str() {
            Ok(s) if s.trim() == "*" => true,
            Ok(s) => s.split(',').any(|t| t.trim() == etag),
            Err(_) => false,
        },
        None => false,
    }
}

pub(crate) fn file_response(file: &Path, body: Vec<u8>) -> Response {
    let mut r = Response::new(axum::body::Body::from(body));
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_str(mime_of(file)).expect("valid mime"),
    );
    r
}

/// Capture → axum Response（status/headers/body 原样回写）。
/// 流式响应（v0.1.35）：`cap.stream` 为 Some 时以 `Body::from_stream` 逐块写出，绕过信封。
pub(crate) fn parse_range(header: Option<&axum::http::HeaderValue>, len: usize) -> RangeSpec {
    let Some(h) = header else {
        return RangeSpec::Full;
    };
    let Ok(s) = h.to_str() else {
        return RangeSpec::Full;
    };
    let Some(rest) = s.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    // ponytail: 多区间（`bytes=a-b,c-d`）→ 200 全量。
    if rest.contains(',') {
        return RangeSpec::Full;
    }
    let (a, b) = match rest.split_once('-') {
        Some(v) => v,
        None => return RangeSpec::Full,
    };
    let parse = |s: &str| s.trim().parse::<usize>().ok();
    match (parse(a), parse(b)) {
        // "a-b"
        (Some(start), Some(end)) if start <= end => {
            if len == 0 || start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(start, end.min(len - 1))
            }
        }
        // "a-"（到末尾）
        (Some(start), None) => {
            if len == 0 || start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(start, len - 1)
            }
        }
        // "-N"（末尾 N 字节）
        (None, Some(n)) => {
            if len == 0 || n == 0 {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(len.saturating_sub(n), len - 1)
            }
        }
        _ => RangeSpec::Full,
    }
}

/// oj-8：自定义响应头补缺——**只插入响应里还没有的头**（框架自有头 Content-Type/
/// Content-Length/Location/Content-Range 等永远优先）；非法头名/值静默跳过
/// （与 capture_response 对 cap.headers 的宽容解析同款语义）。
pub(crate) fn site_headers<'a>(st: &'a AppState, site: &'a WebMount) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = st.static_opts.response_headers.clone();
    for (k, v) in &site.headers {
        out.retain(|(ek, _)| ek != k);
        out.push((k.clone(), v.clone()));
    }
    out
}
