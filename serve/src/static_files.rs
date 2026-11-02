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
        // stat 先行（spec D17）：ETag / min_length 门控 / 缓存键同一次 metadata 取齐，
        // 原文只在需要的腿懒读（缓存命中腿不读盘）。
        if let Ok(m) = tokio::fs::metadata(&file).await {
            let opts = st.static_opts.static_layer.as_ref();
            let enc = if opts.is_some() {
                negotiate_encoding(headers)
            } else {
                Encoding::Identity
            };
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            let compressible = opts.is_some_and(|o| is_compressible(ext, m.len(), o));
            // 非 HTML 才有 ETag（HTML 体经 meta 注入后与文件不再逐字节一致）；可压资源
            // 的 br/gzip 变体以弱 ETag 服务（spec D7，变体选择先于 INM 校验）。
            let etag = (!is_html).then(|| etag_of(&m)).flatten();
            let weak = compressible && enc != Encoding::Identity;
            let served_etag = etag
                .as_deref()
                .map(|e| if weak { format!("W/{e}") } else { e.to_string() });
            if let Some(e) = &served_etag
                && if_none_match_hit(headers, e)
            {
                let mut r = Response::new(axum::body::Body::empty());
                *r.status_mut() = StatusCode::NOT_MODIFIED;
                if let Ok(v) = axum::http::HeaderValue::from_str(e) {
                    r.headers_mut().insert(axum::http::header::ETAG, v);
                }
                finish_static_headers(st, w, &mut r, compressible, &file);
                return r;
            }
            // 压缩腿（仅非 HTML）：缓存直出或现场压——原文不读（懒读收益腿）。
            if !is_html
                && compressible
                && enc != Encoding::Identity
                && let Some(o) = opts
                && let Some(out) = compressed_body_or_cache(&file, &m, enc, o).await
            {
                let out_len = out.len();
                tracing::debug!(
                    target: "serve::static",
                    algo = if enc == Encoding::Br { "br" } else { "gzip" },
                    from = m.len(), to = out_len, file = %file.display()
                );
                let mut r = Response::new(axum::body::Body::from(out));
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_str(mime_of(&file)).expect("valid mime"),
                );
                r.headers_mut().insert(
                    axum::http::header::CONTENT_ENCODING,
                    axum::http::HeaderValue::from_static(match enc {
                        Encoding::Br => "br",
                        Encoding::Gzip => "gzip",
                        Encoding::Identity => "identity",
                    }),
                );
                if let Some(e) = &served_etag
                    && let Ok(v) = axum::http::HeaderValue::from_str(e)
                {
                    r.headers_mut().insert(axum::http::header::ETAG, v);
                }
                if let Ok(v) = axum::http::HeaderValue::from_str(&out_len.to_string()) {
                    r.headers_mut().insert(axum::http::header::CONTENT_LENGTH, v);
                }
                finish_static_headers(st, w, &mut r, compressible, &file);
                return r;
            }
            // identity / 无收益 / HTML：读原文（懒读兜底腿）。
            if let Ok(body) = tokio::fs::read(&file).await {
                let mut r = if is_html {
                    static_page(st, &w.root, rel_path, &file, body).await
                } else {
                    file_response(&file, body)
                };
                // HTML 压缩只在内存（注入后的体 ≠ 文件字节，不落缓存，spec D3）。
                if is_html
                    && compressible
                    && enc != Encoding::Identity
                    && let Some(o) = opts
                {
                    compress_response(&mut r, enc, o).await;
                }
                if let Some(e) = &served_etag
                    && let Ok(v) = axum::http::HeaderValue::from_str(e)
                {
                    r.headers_mut().insert(axum::http::header::ETAG, v);
                }
                finish_static_headers(st, w, &mut r, compressible, &file);
                return r;
            }
        }
    }
    // SPA 深链接回落（挂载 `spa: true`，v0.1.20 语义）：未命中 + 无扩展名 +
    // Accept html → 送 **本挂载** root/index.html。
    if w.spa && accept_html && !has_extension(rel_path) {
        let idx = w.root.join("index.html");
        if let Ok(body) = tokio::fs::read(&idx).await {
            let body_len = body.len() as u64;
            let mut r = static_page(st, &w.root, rel_path, &idx, body).await;
            let opts = st.static_opts.static_layer.as_ref();
            let enc = if opts.is_some() {
                negotiate_encoding(headers)
            } else {
                Encoding::Identity
            };
            let compressible = opts.is_some_and(|o| is_compressible("html", body_len, o));
            if compressible && enc != Encoding::Identity && let Some(o) = opts {
                compress_response(&mut r, enc, o).await;
            }
            finish_static_headers(st, w, &mut r, compressible, &idx);
            return r;
        }
    }
    fail_response(404, "no route matched")
}

/// 静态腿统一收尾（200/304 共用，DRY）：显式头（先写，兜底可被其缺席检查挡住，
/// spec D8/R2）→ Vary 合并（可压资源恒带，与选中编码无关，spec D11）→
/// 默认缓存策略兜底（T6 两档制）。
fn finish_static_headers(
    st: &AppState,
    w: &WebMount,
    r: &mut Response,
    compressible: bool,
    file: &Path,
) {
    apply_custom_headers(r, &site_headers(st, w));
    if compressible {
        merge_vary_ae(r);
    }
    apply_default_cache_control(r, file, &st.static_opts.web_cache_policy);
}

/// Vary 合并（spec D11/网络复核）：既有值 `*` 独占不追加；已含 accept-encoding 幂等；
/// 否则逗号合并（不clobber CORS 等写入的既有 Vary）。
fn merge_vary_ae(r: &mut Response) {
    let existing = r
        .headers()
        .get(axum::http::header::VARY)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    match existing.as_deref() {
        Some("*") => {}
        Some(v) => {
            let has = v
                .split(',')
                .any(|p| p.trim().eq_ignore_ascii_case("accept-encoding"));
            if !has
                && let Ok(nv) = axum::http::HeaderValue::from_str(&format!("{v}, Accept-Encoding"))
            {
                r.headers_mut().insert(axum::http::header::VARY, nv);
            }
        }
        None => {
            r.headers_mut()
                .insert(axum::http::header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }
    }
}

/// 静态腿体上限（防御 cap；体均出自本层自建的缓冲响应）。
const STATIC_BODY_LIMIT: usize = 1 << 30;

/// 内存直压（HTML 腿：meta 注入后的体 ≠ 文件字节，不落缓存不吃伴生，spec D3）；
/// None（无收益）原样放回。压缩成功：CE + 显式 CL + ETag 弱化（体已 ≠ 文件）。
async fn compress_response(r: &mut Response, enc: Encoding, opts: &StaticLayerOpts) -> bool {
    let (mut parts, body) = std::mem::take(r).into_parts();
    let bytes = match axum::body::to_bytes(body, STATIC_BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => {
            *r = Response::from_parts(parts, axum::body::Body::empty());
            return false;
        }
    };
    let Some(out) = compress_body(enc, bytes.to_vec(), opts).await else {
        *r = Response::from_parts(parts, axum::body::Body::from(bytes));
        return false;
    };
    tracing::debug!(
        target: "serve::static",
        algo = if enc == Encoding::Br { "br" } else { "gzip" },
        from = bytes.len(), to = out.len()
    );
    parts.headers.insert(
        axum::http::header::CONTENT_ENCODING,
        HeaderValue::from_static(match enc {
            Encoding::Br => "br",
            Encoding::Gzip => "gzip",
            Encoding::Identity => "identity",
        }),
    );
    if let Ok(v) = HeaderValue::from_str(&out.len().to_string()) {
        parts.headers.insert(axum::http::header::CONTENT_LENGTH, v);
    }
    if let Some(e) = parts
        .headers
        .get(axum::http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        && !e.starts_with("W/")
        && let Ok(v) = HeaderValue::from_str(&format!("W/{e}"))
    {
        parts.headers.insert(axum::http::header::ETAG, v);
    }
    *r = Response::from_parts(parts, axum::body::Body::from(out));
    true
}

/// 两档制（spec D10）：文件 stem 按 [-._] 分段，**末段** ≥8 字符且同时含数字与字母
/// → immutable（确证内容寻址）；其余一律 no-cache 恒校验（现状对齐，ETag/304 下重验
/// 只花一个 RTT；纯字母词如 `production` 漏过是**特性**——防 config-production.js
/// 被 immutable 钉死一年）。判定只用落盘文件路径，绝不用深链接 rel_path。
fn default_cache_control(file: &Path) -> &'static str {
    const IMMUTABLE: &str = "public, max-age=31536000, immutable";
    let hashed = file
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| {
            stem.rsplit(['-', '_', '.']).next().filter(|seg| {
                seg.len() >= 8
                    && seg.chars().any(|c| c.is_ascii_digit())
                    && seg.chars().any(|c| c.is_ascii_alphabetic())
            })
        })
        .is_some();
    if hashed { IMMUTABLE } else { "no-cache" }
}

/// 默认缓存兜底（spec D8/D10）：显式头缺席时按两档制补 Cache-Control；
/// `none` 策略 = 全不加（v0.1.115 行为）。
fn apply_default_cache_control(r: &mut Response, file: &Path, policy: &CachePolicy) {
    if *policy == CachePolicy::None_
        || r.headers().contains_key(axum::http::header::CACHE_CONTROL)
    {
        return;
    }
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(default_cache_control(file)),
    );
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

/// If-None-Match 命中判定（**弱比较**，v0.1.116 起剥 `W/` 比较 opaque——压缩变体
/// 发弱 ETag 后客户端原样回传，精确串比较会永久 miss；RFC 9110 §8.8.3.2）。
pub(crate) fn if_none_match_hit(headers: &HeaderMap, etag: &str) -> bool {
    fn opaque(e: &str) -> &str {
        e.trim().strip_prefix("W/").unwrap_or(e.trim())
    }
    let target = opaque(etag);
    match headers.get(axum::http::header::IF_NONE_MATCH) {
        Some(v) => match v.to_str() {
            Ok(s) if s.trim() == "*" => true,
            Ok(s) => s.split(',').any(|t| opaque(t) == target),
            Err(_) => false,
        },
        None => false,
    }
}

// —— 压缩内核（v0.1.116 OJ-9）——

/// 硬排除地板集（防呆不可关，`compress_types` 列了也不压）：原生已压缩/图格式，
/// 压缩无收益甚至变大。
pub(crate) const COMPRESS_FLOOR: [&str; 9] =
    ["png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "pdf", "woff2"];

/// 内置可压缩集（`compress_types` 缺省时的默认判定集）。
pub(crate) const COMPRESS_BUILTIN: [&str; 15] = [
    "html", "htm", "css", "js", "mjs", "json", "map", "txt", "md", "svg", "xml", "yaml", "yml",
    "woff", "ttf",
];

/// 协商结果（br > gzip 固定优先级，spec R1/D16——不看 q 值高低）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Encoding {
    Br,
    Gzip,
    Identity,
}

/// Accept-Encoding 协商：`get_all` 合并（HTTP/2 多值头合法）、token 小写化、
/// `;q=` 解析（缺省 1.0，q=0 排除）；仅认显式 `br`/`gzip`，`*` 视为仅提供 gzip；
/// 坏头/无头/未知码 → Identity。
pub(crate) fn negotiate_encoding(headers: &HeaderMap) -> Encoding {
    let mut br = 0.0f32;
    let mut gz = 0.0f32;
    for v in headers.get_all(axum::http::header::ACCEPT_ENCODING) {
        let Ok(s) = v.to_str() else {
            return Encoding::Identity;
        };
        for part in s.split(',') {
            let (tok, q) = match part.split_once(';') {
                Some((t, p)) => (
                    t,
                    p.trim()
                        .strip_prefix("q=")
                        .and_then(|q| q.trim().parse::<f32>().ok())
                        .unwrap_or(1.0),
                ),
                None => (part, 1.0),
            };
            match tok.trim().to_ascii_lowercase().as_str() {
                "br" if q > 0.0 => br = br.max(q),
                "gzip" if q > 0.0 => gz = gz.max(q),
                "*" if q > 0.0 => gz = gz.max(q),
                _ => {}
            }
        }
    }
    if br > 0.0 {
        Encoding::Br
    } else if gz > 0.0 {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

/// 扩展名是否可压：min_length 门 → 地板集恒拦 → 内置/替换集判定。
pub(crate) fn is_compressible(ext: &str, len: u64, opts: &StaticLayerOpts) -> bool {
    if len < opts.compress_min_length {
        return false;
    }
    let e = ext.to_ascii_lowercase();
    if COMPRESS_FLOOR.contains(&e.as_str()) {
        return false;
    }
    match &opts.compress_types {
        None => COMPRESS_BUILTIN.contains(&e.as_str()),
        Some(set) => set.contains(&e),
    }
}

/// 压缩（spawn_blocking 内执行，防 CPU 尖峰占 runtime 线程）；None = 压缩后 ≥ 原文
/// （spec D6 负收益防线，调用方出 identity）。br：quality/lgwin=22；gzip：level。
pub(crate) async fn compress_body(
    enc: Encoding,
    body: Vec<u8>,
    opts: &StaticLayerOpts,
) -> Option<Vec<u8>> {
    let (q_br, q_gz) = (opts.brotli_quality as u32, opts.gzip_level as u32);
    tokio::task::spawn_blocking(move || match enc {
        Encoding::Br => {
            use std::io::Write;
            let mut out = Vec::with_capacity(body.len() / 4);
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, q_br, 22);
            w.write_all(&body).ok()?;
            drop(w); // drop 触发 flush 收尾
            (out.len() < body.len()).then_some(out)
        }
        Encoding::Gzip => {
            use std::io::Write;
            let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(q_gz));
            w.write_all(&body).ok()?;
            w.finish().ok().filter(|o| o.len() < body.len())
        }
        Encoding::Identity => None,
    })
    .await
    .ok()
    .flatten()
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

// —— 压缩缓存仓库层（v0.1.116 OJ-9）——

/// 同键压缩 single-flight 登记表（键 = cache_key；进程级）。
static INFLIGHT: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 全局压缩并发钳（防冷缓存风暴，spec D13）；permits = 可用并行度。
static COMPRESS_SEM: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| {
        tokio::sync::Semaphore::new(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
        )
    });

/// 缓存键 `{sha1(abs)}-{ino:x}-{mtime_ns:x}-{len:x}-{br|gz}{q}`：任一失效输入变化
/// 自然换代（ino 防 mtime 保留式替换，spec D14）。
fn cache_key(
    file: &Path,
    meta: &std::fs::Metadata,
    enc: Encoding,
    opts: &StaticLayerOpts,
) -> String {
    // 键用**传入路径原文**——canonicalize 随文件存在性变化（存在→/private/var 解析、
    // 删除→原样），会把同一文件拆成两代键（macOS 实测）；调用方传入的已是挂载根
    // 拼接的稳定路径。
    // SHA1 仅作缓存键指纹（非安全面；ring 0.17 的 LEGACY 命名即此用途）。
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        file.to_string_lossy().as_bytes(),
    );
    let sha1 = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>();
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0u64;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{sha1}-{ino:x}-{mtime_ns:x}-{:x}-{}{}",
        meta.len(),
        match enc {
            Encoding::Br => "br",
            Encoding::Gzip => "gz",
            Encoding::Identity => "id",
        },
        match enc {
            Encoding::Br => opts.brotli_quality,
            Encoding::Gzip => opts.gzip_level,
            Encoding::Identity => 0,
        }
    )
}

/// leader 退出即清登记并唤醒 follower（notify_waiters + follower 端 leader 存在性
/// 复查闭环竞态——先注册再等，醒来重查缓存）。
struct InflightGuard(String);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(n) = INFLIGHT.lock().unwrap().remove(&self.0) {
            n.notify_waiters();
        }
    }
}

/// 取压缩体（spec D3/D13/D14）：缓存命中直读（不读原文不压缩）→ miss 经 single-flight
/// 成为 leader：信号量限流下读原文压缩、原子落盘（tmp+rename，Windows 先 remove）、
/// 删同 `{sha1}-` 前缀旧代；cache_dir 未配或写失败 → 纯内存直压（fail-open）。
/// None = 压缩无收益（D6），调用方出 identity。
pub(crate) async fn compressed_body_or_cache(
    file: &Path,
    meta: &std::fs::Metadata,
    enc: Encoding,
    opts: &StaticLayerOpts,
) -> Option<Vec<u8>> {
    let dir = opts.cache_dir.as_deref();
    let key = cache_key(file, meta, enc, opts);
    async fn cached(d: Option<&Path>, k: &str) -> Option<Vec<u8>> {
        match d {
            Some(d) => tokio::fs::read(d.join(k)).await.ok(),
            None => None,
        }
    }
    if let Some(b) = cached(dir, &key).await {
        return Some(b);
    }
    let wait = {
        let mut m = INFLIGHT.lock().unwrap();
        match m.get(&key) {
            Some(n) => n.clone(),
            None => {
                let n = std::sync::Arc::new(tokio::sync::Notify::new());
                m.insert(key.clone(), n.clone());
                n
            }
        }
    };
    let is_leader = {
        let m = INFLIGHT.lock().unwrap();
        m.get(&key)
            .map(|n| std::sync::Arc::ptr_eq(n, &wait))
            .unwrap_or(false)
    };
    if !is_leader {
        // follower：注册后循环（缓存 → 领导者在否 → 等通知）
        loop {
            if let Some(b) = cached(dir, &key).await {
                return Some(b);
            }
            let fut = wait.notified();
            tokio::pin!(fut);
            if INFLIGHT.lock().unwrap().contains_key(&key) {
                fut.await;
            } else {
                // leader 已完成且缓存未命中（无收益路径）→ 同答 None
                return None;
            }
        }
    }
    let _guard = InflightGuard(key.clone());
    let _permit = COMPRESS_SEM.acquire().await;
    // 双查：等 permit 期间同键可能已被别的 leader 写入
    if let Some(b) = cached(dir, &key).await {
        return Some(b);
    }
    let body = tokio::fs::read(file).await.ok()?;
    let out = match compress_body(enc, body, opts).await {
        Some(o) => o,
        None => {
            eprintln!("DBG compress none");
            return None;
        }
    };
    if let Some(d) = dir {
        let tmp = d.join(format!("{}.tmp-{}", key, std::process::id()));
        if tokio::fs::write(&tmp, &out).await.is_ok() {
            #[cfg(target_os = "windows")]
            let _ = tokio::fs::remove_file(d.join(&key)).await;
            if tokio::fs::rename(&tmp, d.join(&key)).await.is_ok() {
                let prefix = format!("{}-", key.split('-').next().unwrap_or(""));
                let mut rd = match tokio::fs::read_dir(d).await {
                    Ok(rd) => rd,
                    Err(_) => return Some(out),
                };
                while let Ok(Some(e)) = rd.next_entry().await {
                    let name = e.file_name();
                    if name != *key && name.to_string_lossy().starts_with(&prefix) {
                        let _ = tokio::fs::remove_file(e.path()).await;
                    }
                }
            }
        } // 写失败静默：缓存是优化不是正确性依赖
    }
    Some(out)
}

#[cfg(test)]
#[path = "static_files_tests.rs"]
pub(crate) mod tests;
