//! multipart/blob 上传层：流式解析 + 直传 + 守卫。
use super::*;

/// content-type 是否 multipart/form-data。
pub(crate) fn is_multipart(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/form-data"))
}

/// 挂断联动守卫（REQ-11 G2）：客户端断连/超时取消导致 handler future 被 drop 时，
/// 经 cancel token 触达该请求登记的在飞池执行（cancel_by_token → close_on_drop 真杀）。
/// 正常完成路径先 `disarm()`。语义同 [`PutStreamGuard`]（客户端断开 ≠ 查询该跑完）。
pub(crate) struct DisconnectGuard {
    pub(crate) token: u64,
    pub(crate) registry: Arc<only_js::bridge::db_exec::ExecRegistry>,
    pub(crate) armed: bool,
}

impl DisconnectGuard {
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        if self.armed {
            let registry = Arc::clone(&self.registry);
            let token = self.token;
            // Drop 发生在 axum/hyper 的 runtime 线程上（取消即就地 drop future）——spawn 可用。
            // cancel_by_token 只置 flag + notify：在飞 select! 分支 drop fetch 并 close_on_drop。
            tokio::spawn(async move {
                registry.cancel_by_token(token);
            });
        }
    }
}

/// 请求级 cancel token 源（REQ-11 G2）：进程级单调分配；0 预留给「无 token」语义外的
/// 未来扩展，故从 1 起。token 随 RequestInfo 进桥，池执行登记时记入 PoolExecShared。
pub(crate) static CANCEL_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 流式上传会话守卫（v0.1.84）：pump future 被丢弃（客户端断连/超时取消）时
/// Drop fire `put_stream_abort`——s3 multipart 不 abort 会留 orphan parts 烧钱，
/// 且插件侧会话条目永久泄漏（v0.1.38 起两路 pump 只覆盖显式错误路径）。
/// 显式 abort/finish 前先 `disarm()`；abort 本身幂等（未知 id 返回 Err 被丢弃）。
pub(crate) struct PutStreamGuard {
    blob: Arc<dyn BlobBackend>,
    upload_id: u64,
    armed: bool,
}

impl PutStreamGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PutStreamGuard {
    fn drop(&mut self) {
        if self.armed {
            let blob = Arc::clone(&self.blob);
            let id = self.upload_id;
            // Drop 发生在 axum/hyper 的 runtime 线程上（取消即就地 drop future）——spawn 可用。
            tokio::spawn(async move {
                let _ = blob.put_stream_abort(id).await;
            });
        }
    }
}

/// blob 直传 PUT 的执行体（v0.1.38）：流式 `put_stream_*` 优先（内存恒定，总量闸
/// blob_upload_max，超限 abort + 信封 413）；后端不支持流式（put_stream_open Err，
/// 如旧形态后端/网关类实现）→ 回落整段缓冲 put（上限 blob_upload_max，语义同旧版）。
pub(crate) async fn blob_put_direct(
    st: &AppState,
    blob: std::sync::Arc<dyn only_js::bridge::BlobBackend>,
    key: &str,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Response {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let ok_resp = || {
        let mut r = Response::new(axum::body::Body::from(only_js::bridge::ok(
            &serde_json::Value::Null,
        )));
        *r.status_mut() = StatusCode::OK;
        r.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        apply_custom_headers(&mut r, &st.static_opts.response_headers);
        r
    };
    match blob.put_stream_open(key, ct.as_deref()).await {
        Ok(upload_id) => {
            let mut guard = PutStreamGuard {
                blob: Arc::clone(&blob),
                upload_id,
                armed: true,
            };
            let mut total: u64 = 0;
            let mut too_large = false;
            let mut err: Option<String> = None;
            let mut ds = body.into_data_stream();
            use futures_util::StreamExt;
            while let Some(chunk) = ds.next().await {
                match chunk {
                    Ok(b) => {
                        total += b.len() as u64;
                        if total > st.pipeline.blob_upload_max {
                            too_large = true;
                            break;
                        }
                        if let Err(e) = blob.put_stream_chunk(upload_id, &b).await {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                    Err(e) => {
                        err = Some(format!("read body: {e}"));
                        break;
                    }
                }
            }
            if too_large || err.is_some() {
                guard.disarm();
                let _ = blob.put_stream_abort(upload_id).await;
                if too_large {
                    return fail_response(413, "upload too large");
                }
                tracing::warn!(
                    key,
                    err = err.as_deref().unwrap_or("?"),
                    "blob streaming upload failed"
                );
                return fail_response(500, "blob upload failed");
            }
            guard.disarm();
            match blob.put_stream_finish(upload_id).await {
                Ok(()) => ok_resp(),
                Err(_) => fail_response(500, "blob upload failed"),
            }
        }
        // 回落：整段缓冲（上限 blob_upload_max）。
        Err(_) => match axum::body::to_bytes(body, st.pipeline.blob_upload_max as usize).await {
            Ok(b) => match blob.put(key, &b, ct.as_deref()).await {
                Ok(()) => ok_resp(),
                Err(_) => fail_response(500, "blob upload failed"),
            },
            Err(_) => fail_response(413, "upload too large"),
        },
    }
}

/// 流式上传的对象 key（服务端代分配；handler 经 `http.files[i].key`/`.url` 取）。
/// `uploads/<ns 时间戳>-<进程内序号>-<安全化文件名>`——段非空、无穿越字符（valid_key 直通）。
pub(crate) fn gen_upload_key(filename: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let safe: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches(|c| c == '.' || c == '_').to_string();
    let safe = if safe.is_empty() {
        "file.bin".to_string()
    } else {
        safe
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "uploads/{nanos:x}-{}-{safe}",
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// multer 错误 → 响应：超限 413（FieldSizeExceeded / StreamSizeExceeded），其余 400。
pub(crate) fn multipart_err_response(e: multer::Error) -> Response {
    match &e {
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            fail_response(413, "upload too large")
        }
        _ => fail_response(400, &format!("multipart parse: {e}")),
    }
}

/// ABI 10 流式 multipart 解析（v0.1.38）：
/// - 文本字段：缓冲并入 body（{name: value}），累计 ≤ max_upload（防内存 DoS）；
/// - 文件字段 ≤ max_upload：缓冲进 bytes（`http.file(i)` 旧行为不变）；
/// - 文件字段 > max_upload：**转流式** `put_stream_*` 直落 blob（单文件上限
///   blob_upload_max），bytes 置空、`key`/`url` 回填（大文件走 `blob.get(key)` /
///   `files[i].url`）；任意失败 abort 会话（防 local 临时文件泄漏 / s3 orphan parts）。
///   multer 总闸 = max_upload + blob_upload_max；multer 报超限 → 413。
pub(crate) async fn stream_multipart(
    st: &AppState,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Result<(Vec<u8>, Vec<UploadedFile>), Box<Response>> {
    let boundary = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|b| b.trim().trim_matches('"'))
        .unwrap_or_default();
    let blob = st
        .pipeline
        .blob
        .as_ref()
        .expect("caller checked blob configured");
    let max_up = st.pipeline.max_upload;
    let blob_cap = st.pipeline.blob_upload_max;
    let constraints = multer::Constraints::new().size_limit(
        multer::SizeLimit::new()
            .whole_stream(max_up + blob_cap)
            .per_field(blob_cap),
    );
    let mut mp =
        multer::Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let mut fields = serde_json::Map::new();
    let mut files = Vec::new();
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(multipart_err_response)
        .map_err(Box::new)?
    {
        let name = field.name().unwrap_or_default().to_string();
        let filename = field.file_name().unwrap_or_default().to_string();
        let content_type = field.content_type().map(|c| c.to_string());
        if filename.is_empty() {
            // 文本字段：缓冲（累计 ≤ max_upload；multer per_field 闸是 blob_cap，
            // 文本内存闸以本计数为准，先到先拦）。
            let mut buf: Vec<u8> = Vec::new();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if buf.len() as u64 + chunk.len() as u64 > max_up {
                            return Err(Box::new(fail_response(413, "upload too large")));
                        }
                        buf.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(Box::new(multipart_err_response(e))),
                }
            }
            fields.insert(
                name,
                serde_json::Value::String(String::from_utf8_lossy(&buf).into_owned()),
            );
        } else {
            // 文件字段：缓冲到 max_upload 为止；超过转流式（blob_cap 闸）。
            let mut buf: Vec<u8> = Vec::new();
            let mut session: Option<(u64, String)> = None;
            // v0.1.84：session 在场即持守卫——future 被丢弃（客户端断连/取消）时 Drop
            // 兜底 abort（s3 multipart 不 abort 留 orphan parts）；显式 abort/finish 前 disarm。
            let mut guard: Option<PutStreamGuard> = None;
            let mut total: u64 = 0;
            let mut too_large = false;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        total += chunk.len() as u64;
                        if total > blob_cap {
                            too_large = true;
                            break;
                        }
                        if session.is_none() && buf.len() as u64 + chunk.len() as u64 > max_up {
                            // 缓冲阈值越过 → 转流式：开 session + flush 已缓冲字节
                            let key = gen_upload_key(&filename);
                            match blob.put_stream_open(&key, content_type.as_deref()).await {
                                Ok(id) => {
                                    if let Err(e) = blob.put_stream_chunk(id, &buf).await {
                                        if let Some(g) = guard.as_mut() {
                                            g.disarm();
                                        }
                                        let _ = blob.put_stream_abort(id).await;
                                        return Err(Box::new(fail_response(
                                            500,
                                            &format!("blob upload failed: {e}"),
                                        )));
                                    }
                                    buf = Vec::new();
                                    session = Some((id, key));
                                    guard = Some(PutStreamGuard {
                                        blob: Arc::clone(blob),
                                        upload_id: id,
                                        armed: true,
                                    });
                                }
                                Err(_) => {
                                    // 后端不支持流式且文件 > max_upload：无可回落面（
                                    // 缓冲会突破内存闸）→ 413 拒绝
                                    return Err(Box::new(fail_response(413, "upload too large")));
                                }
                            }
                        }
                        if let Some((id, _)) = session.as_ref() {
                            if let Err(e) = blob.put_stream_chunk(*id, &chunk).await {
                                if let Some(g) = guard.as_mut() {
                                    g.disarm();
                                }
                                let _ = blob.put_stream_abort(*id).await;
                                return Err(Box::new(fail_response(
                                    500,
                                    &format!("blob upload failed: {e}"),
                                )));
                            }
                        } else {
                            buf.extend_from_slice(&chunk);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if let Some(g) = guard.as_mut() {
                            g.disarm();
                        }
                        if let Some((id, _)) = session.take() {
                            let _ = blob.put_stream_abort(id).await;
                        }
                        return Err(Box::new(multipart_err_response(e)));
                    }
                }
            }
            if too_large {
                if let Some(g) = guard.as_mut() {
                    g.disarm();
                }
                if let Some((id, _)) = session.take() {
                    let _ = blob.put_stream_abort(id).await;
                }
                return Err(Box::new(fail_response(413, "upload too large")));
            }
            if let Some((id, key)) = session {
                if let Some(g) = guard.as_mut() {
                    g.disarm();
                }
                // 流式完成：bytes 不回填，key/url 交给 handler
                match blob.put_stream_finish(id).await {
                    Ok(()) => {
                        let url = blob.url(&key).await.ok();
                        files.push(UploadedFile {
                            field: name,
                            filename,
                            content_type,
                            bytes: Vec::new(),
                            size: total,
                            key: Some(key),
                            url,
                        });
                    }
                    Err(e) => {
                        return Err(Box::new(fail_response(
                            500,
                            &format!("blob upload failed: {e}"),
                        )));
                    }
                }
            } else {
                // 小文件（≤ max_upload）：旧行为，bytes 回填
                files.push(UploadedFile {
                    field: name,
                    filename,
                    content_type,
                    bytes: buf,
                    size: total,
                    key: None,
                    url: None,
                });
            }
        }
    }
    Ok((serde_json::to_vec(&fields).unwrap_or_default(), files))
}

/// multer 解析：文本字段 → {name: value}，文件 → Vec<UploadedFile>。
/// body 已整体在内存（DefaultBodyLimit 上限内），用 once stream 喂 multer。
/// （v0.1.38 起仅 blob 未配置/小文件路径经此；流式路径见 `stream_multipart`。）
pub(crate) async fn parse_multipart(
    headers: &HeaderMap,
    body: &[u8],
) -> (Vec<u8>, Vec<UploadedFile>) {
    let boundary = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|b| b.trim().trim_matches('"'))
        .unwrap_or_default();
    let stream = futures_util::stream::once(async move {
        Ok::<_, multer::Error>(axum::body::Bytes::from(body.to_vec()))
    });
    let mut mp = multer::Multipart::new(stream, boundary);
    let mut fields = serde_json::Map::new();
    let mut files = Vec::new();
    while let Some(f) = mp.next_field().await.ok().flatten() {
        let name = f.name().unwrap_or_default().to_string();
        let filename = f.file_name().unwrap_or_default().to_string();
        let content_type = f.content_type().map(|s| s.to_string());
        let bytes = f.bytes().await.unwrap_or_default().to_vec();
        if filename.is_empty() {
            fields.insert(
                name,
                Value::String(String::from_utf8_lossy(&bytes).into_owned()),
            );
        } else {
            files.push(UploadedFile {
                field: name,
                filename,
                content_type,
                size: bytes.len() as u64,
                bytes,
                key: None,
                url: None,
            });
        }
    }
    (serde_json::to_vec(&fields).unwrap_or_default(), files)
}
