//! meta 注入层：静态页 head meta 注入 + meta 缓存 + 冲突剥离。
use super::*;

/// 内部派发 `server.html_meta_handler`（GET）→ (键值表, 可选 cache_control)。
///
/// **不经前置守卫**：页面请求（爬虫 / IM 预览）带不了 `Authorization`，走守卫等于恒 401；
/// 故该 handler 不需要进 `anonymous_paths`（外部直接访问它仍受守卫约束，这正是想要的）。
///
/// **恒以匿名身份运行**（`tenant_id = None` + `anonymous = true`，租户头/请求头/请求体一律
/// 不传递）——这不是省事，是安全边界：页面请求带什么头由客户端决定，若把 `X-Tenant` 透传进来，
/// ①`sql_guard: deny` 下构造器会把**攻击者指定的租户**当过滤条件（匿名访客即可读某租户的行并
/// 写进公开 meta），②`db.asTenant` 的三道门禁要求 `tenant_id.is_none()`，透传会让它必抛。
/// 于是「按租户取数」只有一条正路：handler 从 URL 派生 id 后调 `db.asTenant(id)`
/// （需 `tenant.allow_as_tenant: true`）。`http.tenantId` 在本 handler 里恒 `null`。
///
/// 失败（非法路径 / 不在路由表 / 非 2xx / 超时 / 非 JSON / 信封 `code != 0`）→ Err，
/// 调用点 WARN 后按静态结果送出。
pub(crate) async fn dispatch_meta_handler(
    st: &AppState,
    rel_path: &str,
) -> Result<(serde_json::Map<String, Value>, Option<String>), String> {
    let handler = st
        .static_opts
        .html_meta_handler
        .as_deref()
        .ok_or("not configured")?;
    let norm = crate::routes::normalize(handler)
        .ok_or_else(|| format!("{handler:?} 不是合法路径（须以 / 开头）"))?;
    // 按前缀定挂载（v0.1.58 多 api 挂载）：handler 是带挂载前缀的完整路由路径，
    // 取其所属 api 挂载的路由表查找。
    let table = st
        .mounts
        .iter()
        .find_map(|m| match m {
            RuntimeMount::Api { prefix, table, .. }
                if strip_app_prefix(prefix, &norm).is_some() =>
            {
                Some(table)
            }
            _ => None,
        })
        .ok_or_else(|| format!("{handler:?} 不在任何 api 挂载前缀下（拼错了？）"))?;
    let (file, params, chain) = match table.lookup(&norm, "GET") {
        Lookup::Hit {
            file,
            params,
            handler,
            ..
        } => (file, params, handler),
        Lookup::Conflict(m) => return Err(format!("{handler:?} 路由冲突：{m}")),
        Lookup::MethodNotAllowed => return Err(format!("{handler:?} 未映射 GET 方法")),
        Lookup::NotFound => return Err(format!("{handler:?} 不在路由表（拼错了？）")),
    };
    let mut query = HashMap::new();
    query.insert("path".to_string(), rel_path.to_string());
    let req = RequestInfo {
        method: "GET".to_string(),
        params,
        query,
        headers: HashMap::new(),
        body: Vec::new(),
        body_binary: false,
        tenant_id: None,
        anonymous: true,
        user: None,
        files: Vec::new(),
        bus_tx: None,
        cancel_token: None, // meta 内部合成派发：无独立客户端连接，不挂挂断联动
    };
    let cap = st
        .actor
        .run_module(
            file,
            // handler 链（v0.1.64：meta handler 也可以是嵌套组 verb；顶层即 "get"）——
            // `run_module` 用它逐段索引 default，传 HTTP 动词 "GET" 会找不到 handler。
            chain, req, st.timeout,
            // 内部合成派发：不是外部输入，契约校验失败只会静默降级页面 meta。
            false,
        )
        .await
        .map_err(|e| {
            if e.timeout {
                format!("{handler:?} 超时")
            } else {
                format!("{handler:?} 执行失败：{}", e.msg)
            }
        })?;
    if !(200..300).contains(&cap.status) {
        return Err(format!("{handler:?} 返回 HTTP {}", cap.status));
    }
    let mut v: Value = serde_json::from_slice(&cap.body)
        .map_err(|e| format!("{handler:?} 返回非 JSON 体：{e}"))?;
    // 标准信封 {code,msg,data}：code != 0 视为失败；data 是对象则用它，否则用顶层对象。
    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("failed");
            return Err(format!("{handler:?} 返回 code {code}: {msg}"));
        }
        if let Some(data) = v.get_mut("data") {
            v = data.take();
        }
    }
    let Value::Object(mut obj) = v else {
        return Err(format!("{handler:?} 的返回不是 JSON 对象"));
    };
    // `cache_control` 是保留键（不进 `<head>`）：仅作本响应的 Cache-Control 覆盖。
    // 类型错**只丢这个键**（其余 title/og 照旧注入）——一个笔误不该让整份 meta 消失。
    let cc = match obj.remove("cache_control") {
        Some(Value::String(s)) => Some(s),
        Some(other) => {
            eprintln!("warn: {handler:?} 的 cache_control 不是字符串（已忽略该键）：{other}");
            None
        }
        None => None,
    };
    Ok((obj, cc))
}

/// `server.html_cache_control` / handler 的 `cache_control` → 响应头。
/// 非法头值只 WARN 并忽略（不能让一个配置笔误把页面打成 500）。
pub(crate) fn set_cache_control(r: &mut Response, value: &str) {
    match axum::http::HeaderValue::from_str(value) {
        Ok(hv) => {
            r.headers_mut()
                .insert(axum::http::header::CACHE_CONTROL, hv);
        }
        Err(_) => eprintln!("warn: illegal Cache-Control value {value:?} (ignored)"),
    }
}

/// 静态 meta JSON（`<root>/<dir>/<path>.json`）→ 键值表；未配置/未命中/非对象 → 空表。
/// meta JSON 进程缓存（perf F10）：(mtime, map)。命中只付 1 次 stat——此前每个
/// HTML 请求都在 async worker 线程上做**同步** open/read + JSON parse（worker 被
/// 磁盘往返阻塞）。失效同 transpile 缓存：mtime 变即重读。
/// meta JSON 缓存条目：(mtime, 解析结果)。
pub(crate) type MetaEntry = (
    std::time::SystemTime,
    std::sync::Arc<serde_json::Map<String, Value>>,
);
pub(crate) type MetaCacheMap = HashMap<PathBuf, MetaEntry>;

pub(crate) fn meta_cache() -> &'static std::sync::Mutex<MetaCacheMap> {
    static C: std::sync::OnceLock<std::sync::Mutex<MetaCacheMap>> = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub(crate) async fn load_static_meta(
    root: &Path,
    html_meta: Option<&str>,
    rel_path: &str,
) -> serde_json::Map<String, Value> {
    let Some(dir) = html_meta else {
        return serde_json::Map::new();
    };
    let Some(json_path) = meta_json_path(root, dir, rel_path) else {
        return serde_json::Map::new();
    };
    let mtime = tokio::fs::metadata(&json_path)
        .await
        .and_then(|m| m.modified())
        .ok();
    let Some(mtime) = mtime else {
        return serde_json::Map::new();
    };
    if let Some((t, m)) = meta_cache().lock().unwrap().get(&json_path)
        && *t == mtime
    {
        return (**m).clone();
    }
    let Ok(raw) = tokio::fs::read_to_string(&json_path).await else {
        return serde_json::Map::new();
    };
    let map = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(o)) => std::sync::Arc::new(o),
        Ok(_) => return serde_json::Map::new(),
        Err(_) => {
            eprintln!("warn: html_meta {}: invalid JSON", json_path.display());
            return serde_json::Map::new();
        }
    };
    meta_cache()
        .lock()
        .unwrap()
        .insert(json_path, (mtime, map.clone()));
    (*map).clone()
}

/// 键值表 → `<head>` 注入片段（白名单：`title` / `description` / `canonical` /
/// `og:*`（`property`）/ `twitter:*`（`name`））。只注入 `<title>`/`<meta>`/`<link>`，
/// **绝不注入脚本**（静态响应拿不到 CSP nonce）；值一律 HTML 转义；其余键忽略。
pub(crate) fn render_meta_tags(obj: &serde_json::Map<String, Value>) -> String {
    let mut tags = String::new();
    if let Some(t) = obj.get("title").and_then(|t| t.as_str()) {
        tags.push_str(&format!("<title>{}</title>\n", escape_html(t)));
    }
    if let Some(d) = obj.get("description").and_then(|d| d.as_str()) {
        tags.push_str(&format!(
            "<meta name=\"description\" content=\"{}\">\n",
            escape_html(d)
        ));
    }
    if let Some(c) = obj.get("canonical").and_then(|c| c.as_str()) {
        tags.push_str(&format!(
            "<link rel=\"canonical\" href=\"{}\">\n",
            escape_html(c)
        ));
    }
    // og:* / twitter:*：`og:*` 用 property，其余用 name。
    let mut rest: Vec<(&String, &Value)> = obj
        .iter()
        .filter(|(k, _)| k.starts_with("og:") || k.starts_with("twitter:"))
        .collect();
    rest.sort_by(|a, b| a.0.cmp(b.0)); // 稳定输出（便于测试与 diff）
    for (k, val) in rest {
        if let Some(s) = val.as_str() {
            let attr = if k.starts_with("og:") {
                "property"
            } else {
                "name"
            };
            tags.push_str(&format!(
                "<meta {attr}=\"{}\" content=\"{}\">\n",
                escape_html(k),
                escape_html(s)
            ));
        }
    }
    tags
}

/// 注入键 → 需要**摘掉**的既有标签形态（决定「谁跟谁同名」）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Conflict {
    /// `<title>…</title>`
    Title,
    /// `<link rel="canonical">`
    CanonicalLink,
    /// `<meta name|property="KEY">`
    MetaTag(String),
}

impl Conflict {
    fn matches(&self, name: &str, attrs: &str) -> bool {
        match self {
            Conflict::Title => name.eq_ignore_ascii_case("title"),
            Conflict::CanonicalLink => {
                name.eq_ignore_ascii_case("link")
                    && attr_value(attrs, "rel").is_some_and(|v| v.eq_ignore_ascii_case("canonical"))
            }
            Conflict::MetaTag(key) => {
                name.eq_ignore_ascii_case("meta")
                    && ["name", "property"]
                        .iter()
                        .any(|a| attr_value(attrs, a).is_some_and(|v| v.eq_ignore_ascii_case(key)))
            }
        }
    }
}

/// 键值表 → 「要摘的既有标签」清单（与 `render_meta_tags` 的渲染判据逐条对齐：
/// 只对**字符串值**的键摘旧的，否则会摘掉却不补上）。
pub(crate) fn conflict_keys(obj: &serde_json::Map<String, Value>) -> Vec<Conflict> {
    let text = |k: &str| obj.get(k).and_then(|v| v.as_str()).is_some();
    let mut out = Vec::new();
    if text("title") {
        out.push(Conflict::Title);
    }
    if text("description") {
        out.push(Conflict::MetaTag("description".to_string()));
    }
    if text("canonical") {
        out.push(Conflict::CanonicalLink);
    }
    for k in obj.keys() {
        if (k.starts_with("og:") || k.starts_with("twitter:")) && text(k) {
            out.push(Conflict::MetaTag(k.clone()));
        }
    }
    out
}

/// 把键值表注入 `<head>`：**先摘掉 head 里同名的既有标签**，再在 `</head>` 前插入新标签。
///
/// 为什么必须摘：浏览器与爬虫只认**第一个**匹配——壳里写死的 `<title>App</title>` 不摘掉，
/// 注入的第二个 title 根本不会生效（`og:*` 同理）。v0.1.20 只插入不摘除，等于注入了个寂寞。
/// 键白名单与 HTML 转义由 `render_meta_tags` 负责，这里只管「摘同名旧标签 + 放新标签」。
/// 无 `</head>` / 无可渲染键 → 原样返回（不猜结构）。
/// 接管 HTML 所有权（perf F11）：无可注入标签 / 找不到锚点时原样返回——
/// 此前每请求对整份文档做一次 `to_string()` 全量拷贝。
pub(crate) fn inject_head(html: String, obj: &serde_json::Map<String, Value>) -> String {
    let tags = render_meta_tags(obj);
    if tags.is_empty() {
        return html;
    }
    let Some(head_end) = find_ci(&html, b"</head>") else {
        return html;
    };
    // head 内容起点：`<head …>` 开标签之后；没有开标签就从文档头起算（仍只动 head 段内）。
    // 用 ASCII 不敏感扫描（**不** `to_lowercase()`——大小写变换会改字节长度，索引会错位）。
    let head_start = match find_ci(&html, b"<head") {
        Some(i) => match html[i..].find('>') {
            Some(gt) => i + gt + 1,
            None => return html,
        },
        None => 0,
    };
    if head_start > head_end {
        return html;
    }
    let stripped = strip_conflicts(&html[head_start..head_end], &conflict_keys(obj));
    let mut out = String::with_capacity(html.len() + tags.len());
    out.push_str(&html[..head_start]);
    out.push_str(&stripped);
    out.push_str(&tags);
    out.push_str(&html[head_end..]);
    out
}

/// ASCII 不敏感查找（返回**字节**偏移；针头是 ASCII 标签文本，不会落在多字节字符内部）。
pub(crate) fn find_ci(hay: &str, needle: &[u8]) -> Option<usize> {
    let h = hay.as_bytes();
    if needle.is_empty() || h.len() < needle.len() {
        return None;
    }
    h.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// 一个 HTML 元素（供摘除扫描）：`name` = 标签名（特殊情形为 `"!"` = 永不匹配），
/// `attrs` = 开标签内的属性原文，`len` = 该元素在原文里的字节长度（含闭合标签）。
pub(crate) struct Element<'a> {
    name: &'a str,
    attrs: &'a str,
    len: usize,
}

/// 从 `<` 处解析一个元素；解析不出（未闭合等）→ `None`（调用点放弃摘除，原样保留）。
///
/// `<!…>` / `<?…>` / 闭合标签 / 无名标签一律以 `"!"` 返回（跳过、不参与匹配）；
/// `script` / `style` 的内容当**不透明文本**整段跳过（内容里出现 `<title>` 之类的字符串
/// 不得被当成标签摘掉）。
pub(crate) fn element_at(s: &str) -> Option<Element<'_>> {
    if !s.starts_with('<') {
        return None;
    }
    let never = |len: usize| {
        Some(Element {
            name: "!",
            attrs: "",
            len,
        })
    };
    if s.starts_with("<!--") {
        return never(s.find("-->").map(|i| i + 3)?);
    }
    if s.starts_with("<!") || s.starts_with("<?") || s.starts_with("</") {
        return never(s.find('>').map(|i| i + 1)?);
    }
    let name_end = s.as_bytes()[1..]
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'-'))
        .map(|i| i + 1)
        .unwrap_or(s.len());
    if name_end == 1 {
        return never(s.find('>').map(|i| i + 1)?);
    }
    let name = &s[1..name_end];
    let gt = s.find('>')?;
    let attrs = &s[name_end..gt];
    // 不透明内容段 / 需要连闭合标签一起摘的元素
    let opaque = name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style");
    let has_close = opaque || name.eq_ignore_ascii_case("title");
    if has_close {
        let close = format!("</{name}");
        let len = match find_ci(&s[gt + 1..], close.as_bytes()) {
            Some(k) => {
                let after = gt + 1 + k;
                match s[after..].find('>') {
                    Some(g) => after + g + 1,
                    None => gt + 1,
                }
            }
            None => gt + 1,
        };
        return Some(Element {
            name: if opaque { "!" } else { name },
            attrs,
            len,
        });
    }
    Some(Element {
        name,
        attrs,
        len: gt + 1,
    })
}

/// 摘掉 `head` 段里与 `wants` 同名的既有标签（其余原样保留）。
pub(crate) fn strip_conflicts(head: &str, wants: &[Conflict]) -> String {
    if wants.is_empty() {
        return head.to_string();
    }
    let mut out = String::with_capacity(head.len());
    let mut i = 0usize;
    while i < head.len() {
        let Some(rel) = head[i..].find('<') else {
            out.push_str(&head[i..]);
            break;
        };
        let lt = i + rel;
        out.push_str(&head[i..lt]);
        let Some(el) = element_at(&head[lt..]) else {
            // 解析不了就不冒险：剩余原文照抄
            out.push_str(&head[lt..]);
            break;
        };
        if !wants.iter().any(|w| w.matches(el.name, el.attrs)) {
            out.push_str(&head[lt..lt + el.len]);
        }
        i = lt + el.len;
    }
    out
}

/// 取开标签属性值（ASCII 不敏感属性名；值支持 `"`/`'`/裸值）。属性名前必须是词边界，
/// 免得把 `data-name=` 当成 `name=`。
pub(crate) fn attr_value(attrs: &str, key: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let kb = key.as_bytes();
    if kb.is_empty() || b.len() < kb.len() {
        return None;
    }
    for i in 0..=(b.len() - kb.len()) {
        let boundary = i == 0 || (!b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'-');
        if !boundary || !b[i..i + kb.len()].eq_ignore_ascii_case(kb) {
            continue;
        }
        let mut j = i + kb.len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != b'=' {
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        if b[j] == b'"' || b[j] == b'\'' {
            let start = j + 1;
            let end = start + b[start..].iter().position(|c| *c == b[j])?;
            return Some(String::from_utf8_lossy(&b[start..end]).into_owned());
        }
        let end = b[j..]
            .iter()
            .position(|c| c.is_ascii_whitespace())
            .map(|k| j + k)
            .unwrap_or(b.len());
        return Some(String::from_utf8_lossy(&b[j..end]).into_owned());
    }
    None
}

/// meta JSON 路径：`<root>/<dir>/<sanitized path>.json`；空路径 → `index.json`。
/// 段守卫与 resolve_static 同款（防 `%2e%2e` / `..` 走私）。
pub(crate) fn meta_json_path(root: &Path, dir: &str, rel_path: &str) -> Option<PathBuf> {
    let rel = rel_path
        .strip_prefix('/')
        .unwrap_or(rel_path)
        .trim_end_matches('/');
    let mut p = root.to_path_buf();
    p.push(dir);
    if rel.is_empty() {
        p.push("index.json");
        return Some(p);
    }
    for seg in rel.split('/') {
        let s = percent_encoding::percent_decode_str(seg)
            .decode_utf8()
            .ok()?;
        if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0']) {
            return None;
        }
        p.push(s.as_ref());
    }
    p.set_extension("json");
    Some(p)
}

/// HTML 文本/属性值转义（防 meta JSON 里的 `"`/`</title>` 破坏文档结构）。
pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
