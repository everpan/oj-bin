//! API dispatch 层：admit 准入 + run_route/run_capture 执行 + Capture→Response 映射。
use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_route(
    st: &AppState,
    headers: &HeaderMap,
    body: axum::body::Body,
    verb: &str,
    query: HashMap<String, String>,
    path_no_base: Option<&str>,
    full_path: &str,
    file: PathBuf,
    params: HashMap<String, String>,
    handler: String,
) -> Response {
    // v0.1.38（ABI 10）：multipart + blob 已配置 → **流式**解析（文件字段直落 blob，
    // 内存恒定；大文件不再受 max_upload 硬顶约束，单文件上限 blob_upload_max）；
    // 其余（非 multipart，或未配 blob）维持整体缓冲（上限 2x max_upload；超 max_upload
    // 信封 413、超 2x 裸 413——与旧 DefaultBodyLimit 行为逐字节一致）。
    let (body_bytes, files) = if is_multipart(headers) && st.pipeline.blob.is_some() {
        match stream_multipart(st, headers, body).await {
            Ok(v) => v,
            Err(resp) => return *resp,
        }
    } else {
        let buffered =
            match axum::body::to_bytes(body, (st.pipeline.max_upload.saturating_mul(2)) as usize)
                .await
            {
                Ok(b) => b,
                Err(_) => {
                    return Response::builder()
                        .status(StatusCode::PAYLOAD_TOO_LARGE)
                        .body(axum::body::Body::empty())
                        .unwrap_or_else(|_| Response::new(axum::body::Body::empty()));
                }
            };
        if buffered.len() > st.pipeline.max_upload as usize {
            return fail_response(413, "upload too large");
        }
        if is_multipart(headers) {
            parse_multipart(headers, &buffered).await
        } else {
            (buffered.to_vec(), Vec::new())
        }
    };
    match run_capture(
        st,
        headers,
        verb,
        query,
        path_no_base,
        full_path,
        file,
        params,
        handler,
        body_bytes,
        files,
    )
    .await
    {
        Ok(cap) => {
            let mut resp = capture_response(cap);
            apply_custom_headers(&mut resp, &st.static_opts.response_headers);
            resp
        }
        // 准入拒绝（401/400）/ 超时熔断（408）/ 执行失败（500）：fail 语义与
        // 拆分前一致（这些路径从不施加自定义响应头）。
        Err(e) => fail_response(i32::from(e.status), &e.msg),
    }
}

/// 合成准入失败的 RunFail（401/400 信封体很小，直接读出 msg 供 MCP isError 使用）。
pub(crate) async fn run_fail_from_response(resp: Response) -> RunFail {
    let status = resp.status().as_u16();
    let msg = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| "request rejected".to_string());
    RunFail { status, msg }
}

/// 共享 dispatch（MCP 双线路 R2）：admit + RequestInfo 合成 + route_timeouts 首个
/// 命中 + run_module(validate=true)。run_route（axum 缝合层）与 MCP/JSON-RPC handler
/// **唯一**分发入口——准入/租户/超时/契约校验语义单一，两处实现不得漂移。
///
/// 外部输入一律 validate=true（`src/bridge/mod.rs` 的既定红线）；框架内部合成派发
/// （dispatch_meta_handler 的 meta 注入）不在此入口，仍走其私有路径。
#[allow(clippy::too_many_arguments)] // 分发入参即请求的全部材料；打包成结构体只会加构造样板
pub(crate) async fn run_capture(
    st: &AppState,
    headers: &HeaderMap,
    verb: &str,
    query: HashMap<String, String>,
    path_no_base: Option<&str>,
    full_path: &str,
    file: PathBuf,
    params: HashMap<String, String>,
    handler: String,
    body_bytes: Vec<u8>,
    files: Vec<UploadedFile>,
) -> Result<only_js::bridge::Capture, RunFail> {
    // 前置管线（鉴权 + 租户）统一进口：run_route 与 blob 直传 PUT 共用（v0.1.30 抽出，
    // 防两处准入语义漂移）。401/400 以 RunFail 返回（run_route 转为 Response）。
    let (user, tenant_id, anonymous, hdr_map) = match admit(st, headers, verb, path_no_base) {
        Ok(v) => v,
        Err(resp) => return Err(run_fail_from_response(*resp).await),
    };
    // REQ-11 G2：挂断联动——admit 成功后发 token，guard 伴随整个 handler 执行；
    // 客户端断连导致本 future 被 drop 时，guard 触达本请求登记的在飞池执行。
    let mut guard = st.exec_registry.as_ref().map(|reg| DisconnectGuard {
        token: CANCEL_TOKEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        registry: Arc::clone(reg),
        armed: true,
    });
    let req = RequestInfo {
        method: verb.to_string(),
        params,
        query,
        headers: hdr_map.unwrap_or_else(|| {
            headers
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
                .collect()
        }),
        body: body_bytes,
        body_binary: false,
        tenant_id,
        anonymous,
        user,
        files,
        bus_tx: None,
        cancel_token: guard.as_ref().map(|g| g.token),
    };
    // oj-5c：路由级 timeout 覆盖（按声明顺序首个命中；含 base 的全路径匹配）。
    let timeout = st
        .pipeline
        .route_timeouts
        .iter()
        .find(|(p, _)| path_matches(std::slice::from_ref(p), full_path))
        .map(|(_, d)| *d)
        .or(st.timeout);
    let res = st.actor.run_module(file, handler, req, timeout, true).await;
    // 正常完成：解除挂断联动（cancel 已由响应语义决定，不得由 guard 再触达）。
    if let Some(g) = guard.as_mut() {
        g.disarm();
    }
    res.map_err(|e| RunFail {
        status: if e.timeout { 408 } else { 500 },
        msg: e.msg,
    })
}

/// 共享 dispatch 的失败载体：HTTP 状态 + 信封 msg（MCP 侧映射 isError，run_route 侧
/// 转 fail_response）。
pub(crate) struct RunFail {
    pub status: u16,
    pub msg: String,
}

/// 前置管线准入（v0.1.30 抽出）：鉴权（ABI 9 四参守卫）+ 租户提取。
/// Ok((user, tenant_id, anonymous, headers_map))；Err = 已构造的 401/400 响应。
/// headers_map：鉴权已构建的头 map（perf F8，run_route 复用免二次字符串化）；
/// 鉴权未激活为 None。
pub(crate) type AdmitOut = (
    Option<serde_json::Value>,
    Option<String>,
    bool,
    Option<HashMap<String, String>>,
);

pub(crate) fn admit(
    st: &AppState,
    headers: &HeaderMap,
    verb: &str,
    path_no_base: Option<&str>,
) -> Result<AdmitOut, Box<Response>> {
    // 鉴权（base 内非匿名路径必须过守卫 → 401；Ok(None) = 匿名放行）。
    // ABI 9：方法 + 全部请求头 JSON（小写名 → 值，含 cookie）透传守卫——
    // cookie 会话/CSRF 双提交的判定材料都在 headers 里（oj-auth 消费）。
    // 头 map 只在此构建一次（perf F8）：String 化的 map 同时喂 FFI JSON 与
    // run_route 的 RequestInfo（此前同一批头被字符串化两遍）。
    let mut hdr_map: Option<HashMap<String, String>> = None;
    let user = match (st.pipeline.auth.as_ref(), path_no_base) {
        (Some(guard), Some(p)) => {
            let header = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            let hm: HashMap<String, String> = headers
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
                .collect();
            let headers_json = serde_json::to_string(&hm).unwrap_or_default();
            hdr_map = Some(hm);
            match guard.verify(p, verb, header, Some(&headers_json)) {
                Ok(Some(u)) => Some(u),
                Ok(None) => None,
                Err(msg) => return Err(Box::new(fail_response(401, &msg))),
            }
        }
        _ => None,
    };
    // 租户提取（启用后缺失/空 → 400；anonymous_paths 命中的跳转腿
    // 豁免"缺失 400"——OIDC 302 带不了自定义头——但已带的头仍注入）。
    // anonymous：db.asTenant 的授信判据（v0.1.20）。只有「豁免命中 + 确实没带
    // 租户头」才是匿名；带了头或没豁免都不是（后者走 400 / tid 注入）。
    let mut anonymous = false;
    let tenant_id = match st.pipeline.tenant_header.as_deref() {
        Some(key) => {
            let exempt = path_matches(&st.pipeline.tenant_anon, path_no_base.unwrap_or(""));
            let header_tid = headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if st.pipeline.tenant_require_signed_claim {
                // PR-9（v0.1.41）：租户只认**验签后的 claim**（user.claims.tenant）。
                // 裸头不再自证：带头仅作与 claim 的一致性复核（不符 403）；
                // anonymous_paths 豁免保持（OIDC 回跳逃生口不变），其余路径 fail-closed。
                let claim = user
                    .as_ref()
                    .and_then(|u| u.get("claims"))
                    .and_then(|c| c.get("tenant"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                match (&header_tid, &claim) {
                    (Some(h), Some(c)) if h != c => {
                        return Err(Box::new(fail_response(
                            403,
                            "tenant header does not match signed claim",
                        )));
                    }
                    // claim 是唯一来源（头一致或缺失都取 claim）；claim 缺失时头不得自证。
                    (_, Some(c)) => Some(c.clone()),
                    (Some(_), None) => {
                        return Err(Box::new(fail_response(
                            403,
                            "tenant requires a signed claim (user.claims.tenant missing)",
                        )));
                    }
                    (None, None) if exempt => {
                        anonymous = true;
                        None
                    }
                    (None, None) => {
                        return Err(Box::new(fail_response(
                            403,
                            "tenant requires a signed claim (user.claims.tenant missing)",
                        )));
                    }
                }
            } else {
                match header_tid {
                    Some(tid) => Some(tid),
                    None if exempt => {
                        anonymous = true;
                        None
                    }
                    None => {
                        return Err(Box::new(fail_response(
                            400,
                            &format!("missing tenant header: {key}"),
                        )));
                    }
                }
            }
        }
        None => None,
    };
    Ok((user, tenant_id, anonymous, hdr_map))
}

/// SPA 回落的 Accept 判据（v0.1.20）：缺失 / `*/*` / 含 `text/html`（q>0）均视为 html。
/// curl 默认发 `*/*` —— 只认字面 `text/html` 会让回落对最常用客户端失效。
pub(crate) fn capture_response(cap: only_js::bridge::Capture) -> Response {
    let mut r = if let Some(rx) = cap.stream {
        // 接收端逐块收，映射到 `Result<Bytes, io::Error>` 交给 axum（chunked 传输）。
        // 不用 `StreamExt::map`：tokio 1.x 的 mpsc 接收端未实现 `Stream` trait（无 stream
        // feature），改用 unfold + recv() 自建流。
        use futures_util::stream::unfold;
        let s = unfold(rx, |mut rx| async move {
            rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx))
        });
        Response::new(axum::body::Body::from_stream(s))
    } else {
        Response::new(axum::body::Body::from(cap.body))
    };
    *r.status_mut() = StatusCode::from_u16(cap.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (k, v) in cap.headers {
        if let (Ok(name), Ok(hv)) = (
            k.parse::<axum::http::HeaderName>(),
            v.parse::<axum::http::HeaderValue>(),
        ) {
            // Set-Cookie 合法重复（登录双发 oj_sess + oj_csrf 的唯一通道）→ 追加；
            // 其余头保持旧覆盖写语义（同名最后一个生效）。
            if name == axum::http::header::SET_COOKIE {
                r.headers_mut().append(name, hv);
            } else {
                r.headers_mut().insert(name, hv);
            }
        }
    }
    r
}

/// 统一失败信封。
pub(crate) fn fail_response(code: i32, msg: &str) -> Response {
    let (body, status) = fail(code, msg, &Value::Null);
    let mut r = Response::new(axum::body::Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r
}

/// Range 头解析结果（oj-7）：Full = 非区间/非法/多区间（ponytail: 多区间不做
/// multipart/byteranges 响应，回落 200 全量——单区间覆盖 pdf.js/媒体 seek 的全部场景）；
/// Partial(start, end)（含端点，已按 len 钳制）；Unsatisfiable = 越界/空文件 → 416。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RangeSpec {
    Full,
    Partial(usize, usize),
    Unsatisfiable,
}

pub(crate) fn apply_custom_headers(r: &mut Response, extra: &[(String, String)]) {
    for (k, v) in extra {
        if let (Ok(name), Ok(hv)) = (
            k.parse::<axum::http::HeaderName>(),
            axum::http::HeaderValue::from_str(v),
        ) && !r.headers().contains_key(&name)
        {
            r.headers_mut().insert(name, hv);
        }
    }
}

/// 站点级响应头 = 全局默认 + 该站点覆盖（同名替换值，不删全局其它头）。
/// `a=1&b=2` → map（application/x-www-form-urlencoded 解码，+ → 空格、%xx → 字节）。
pub(crate) fn parse_query(q: Option<&str>) -> HashMap<String, String> {
    q.map(|s| {
        form_urlencoded::parse(s.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    })
    .unwrap_or_default()
}
