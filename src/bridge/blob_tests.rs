#[test]
fn blob_registry_multi_backend_and_duplicate_fails() {
    let root = std::env::temp_dir().join(format!("oj-blobreg-{}", std::process::id()));
    let mk = || Arc::new(LocalBlob::new(&root, "/v1/api").unwrap()) as Arc<dyn BlobBackend>;
    let mut r = BlobRegistry::new();
    assert!(r.default().is_none());
    r.register("default", mk()).unwrap();
    r.register("img", mk()).unwrap();
    assert!(r.default().is_some());
    assert!(r.get("img").is_some());
    assert_eq!(r.names(), vec!["default".to_string(), "img".to_string()]);
    // 重名 fail fast
    assert!(r.register("img", mk()).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

use super::*;

use serde_json::Value;

use crate::bridge::{BlobServed, Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
// S3 配置测试随 S3Blob 迁插件（oj-blob-s3，Task 4.2）；core 不再引用 BlobCfg。

fn tmp_root() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "oj-blob-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "current_thread")]
async fn local_roundtrip_and_traversal_rejected() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    b.put("a/b.png", b"PNGDATA", Some("image/png"))
        .await
        .unwrap();
    assert_eq!(b.get("a/b.png").await.unwrap(), b"PNGDATA".to_vec());
    assert_eq!(b.url("a/b.png").await.unwrap(), "/v1/api/blob/a/b.png");
    assert_eq!(
        b.content_type("a/b.png").await.unwrap().as_deref(),
        Some("image/png")
    );
    // 显式非常规 ct 走 sidecar；无 ct 回落扩展名推断
    b.put("x.bin", b"B", Some("application/x-foo"))
        .await
        .unwrap();
    assert_eq!(
        b.content_type("x.bin").await.unwrap().as_deref(),
        Some("application/x-foo")
    );
    b.put("y.png", b"P", None).await.unwrap();
    assert_eq!(
        b.content_type("y.png").await.unwrap().as_deref(),
        Some("image/png")
    );
    b.del("a/b.png").await.unwrap();
    assert!(b.get("a/b.png").await.is_err());
    for bad in ["../x", "a/../b", "", "/abs", "a//b", "a\\b"] {
        assert!(!valid_key(bad), "{bad}");
        assert!(b.put(bad, b"x", None).await.is_err(), "{bad}");
    }
}

#[test]
fn infer_content_type_by_extension() {
    assert_eq!(infer_content_type("a.png"), Some("image/png".into()));
    assert_eq!(infer_content_type("a.JPG"), Some("image/jpeg".into()));
    assert_eq!(infer_content_type("a.svg"), Some("image/svg+xml".into()));
    assert_eq!(infer_content_type("a.pdf"), Some("application/pdf".into()));
    assert_eq!(
        infer_content_type("a.json"),
        Some("application/json".into())
    );
    assert_eq!(infer_content_type("a.mp4"), Some("video/mp4".into()));
    assert_eq!(infer_content_type("a.unknown"), None);
}

/// 非 default local 后端 url() 明确报错（下载路由仅服务 default，spec §2 裁决）。
#[tokio::test(flavor = "current_thread")]
async fn local_blob_url_errors_when_not_default() {
    let root = tmp_root();
    let b = LocalBlob::named("img", &root, "/v1/api").unwrap();
    let e = b
        .url("k")
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        e.contains("only available for the 'default' backend"),
        "{e}"
    );
    // default 名不受影响
    let d = LocalBlob::new(&root, "/v1/api").unwrap();
    assert_eq!(d.url("k").await.unwrap(), "/v1/api/blob/k");
}

/// blob(name) 工厂：命名分发 + default 兼容旧调用 + 未配置名首次调用期报错（spec §2）。
#[tokio::test(flavor = "current_thread")]
async fn blob_named_factory_dispatch() {
    let root_d = tmp_root();
    let root_i = tmp_root();
    let mut reg = BlobRegistry::new();
    reg.register(
        "default",
        Arc::new(LocalBlob::new(&root_d, "/v1/api").unwrap()),
    )
    .unwrap();
    reg.register("img", Arc::new(LocalBlob::new(&root_i, "/v1/api").unwrap()))
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            blobs: Some(Arc::new(reg)),
            ..Default::default()
        },
    );
    // 命名分发：img 与 default 互不串（同名 key 不同内容）
    let cap = b
        .run_with(
            r#"
                (async () => {
                    await blob("img").put("k.txt", new Uint8Array([73, 77, 71]), "text/plain");
                    await blob.put("k.txt", new Uint8Array([68, 69, 70]), "text/plain");
                    const img = Array.from(await blob("img").get("k.txt")).join(",");
                    const def = Array.from(await blob.get("k.txt")).join(",");
                    json.ok({ img, def });
                })().catch((e) => json.fail(500, String(e)));
                "#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["img"], "73,77,71", "{v}");
    assert_eq!(v["data"]["def"], "68,69,70", "{v}");
    assert!(root_i.join("k.txt").is_file() && root_d.join("k.txt").is_file());
    // 未配置名：首次调用期报错（name 入文案）
    let cap = b
            .run_with(
                r#"(async () => { await blob("ghost").get("k"); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["err"]
            .as_str()
            .unwrap()
            .contains("blob backend 'ghost' not configured"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn put_stream_filename_sidecar_roundtrips_for_disposition() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    // 无原名：filename 查询 None
    let id = b
        .put_stream_open("export/a.csv", Some("text/csv"))
        .await
        .unwrap();
    b.put_stream_chunk(id, b"x").await.unwrap();
    b.put_stream_finish(id).await.unwrap();
    assert_eq!(b.filename("export/a.csv").await.unwrap(), None);
    // 有原名：finish 落 sidecar，可回读（下载路由据此下发 Content-Disposition）
    let id2 = b
        .put_stream_open("export/b.csv", Some("text/csv"))
        .await
        .unwrap();
    b.put_stream_filename(id2, "orders-2026.csv").await.unwrap();
    b.put_stream_finish(id2).await.unwrap();
    assert_eq!(
        b.filename("export/b.csv").await.unwrap().as_deref(),
        Some("orders-2026.csv")
    );
    // set_filename（整段 put 回落路径）
    b.put("export/c.csv", b"y", Some("text/csv")).await.unwrap();
    b.set_filename("export/c.csv", "c-export.csv")
        .await
        .unwrap();
    assert_eq!(
        b.filename("export/c.csv").await.unwrap().as_deref(),
        Some("c-export.csv")
    );
    // 第二次流式不带原名：旧 sidecar 被清（不继承）
    let id3 = b
        .put_stream_open("export/b.csv", Some("text/csv"))
        .await
        .unwrap();
    b.put_stream_finish(id3).await.unwrap();
    assert_eq!(b.filename("export/b.csv").await.unwrap(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn local_serve_returns_bytes_and_content_type() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    b.put("d/e.txt", b"hello", Some("text/plain"))
        .await
        .unwrap();
    // serve 直出字节 + content_type
    let sv = b.serve("d/e.txt").await.unwrap();
    match sv {
        BlobServed::Bytes(bytes, ct) => {
            assert_eq!(bytes, b"hello".to_vec());
            assert_eq!(ct.as_deref(), Some("text/plain"));
        }
        BlobServed::Redirect(_) => panic!("local blob must inline-serve"),
    }
    // 越界 key 经 os_path 拒绝
    assert!(b.serve("../up").await.is_err());
}

/// ABI 10 流式上传（local）：chunk append → finish rename 转正 + ct sidecar；
/// abort 删临时文件；未知 upload id 明确报错；非法 key 拒绝。
#[tokio::test(flavor = "current_thread")]
async fn local_put_stream_roundtrip_abort_and_cleanup() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();

    // 非法 key：open 即拒
    assert!(b.put_stream_open("../evil", None).await.is_err());

    // 正常路径：三段 chunk → finish；显式 ct（与推断不同）落 sidecar
    let id = b
        .put_stream_open("up/big.bin", Some("application/x-custom"))
        .await
        .unwrap();
    b.put_stream_chunk(id, b"chunk1-").await.unwrap();
    b.put_stream_chunk(id, b"chunk2").await.unwrap();
    b.put_stream_finish(id).await.unwrap();
    assert_eq!(
        b.get("up/big.bin").await.unwrap(),
        b"chunk1-chunk2".to_vec()
    );
    assert_eq!(
        b.content_type("up/big.bin").await.unwrap().as_deref(),
        Some("application/x-custom")
    );
    // finish 后再 chunk → 未知 id
    assert!(b.put_stream_chunk(id, b"x").await.is_err());

    // abort 路径：临时文件被清理，最终对象不存在
    let id2 = b.put_stream_open("up/aborted.bin", None).await.unwrap();
    b.put_stream_chunk(id2, b"partial").await.unwrap();
    b.put_stream_abort(id2).await.unwrap();
    assert!(b.get("up/aborted.bin").await.is_err());
    // 临时文件名前缀 .oj-tmp-upload- 不残留
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".oj-tmp-upload")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "abort 后临时文件必须清理: {leftovers:?}"
    );

    // 未知 id：finish/abort 明确报错（幂等友好，文案点名）
    let e = b
        .put_stream_finish(999)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("unknown upload"), "{e}");

    // 无 ct 的流式：靠扩展名推断
    let id3 = b.put_stream_open("up/pic.png", None).await.unwrap();
    b.put_stream_chunk(id3, b"PNG").await.unwrap();
    b.put_stream_finish(id3).await.unwrap();
    assert_eq!(
        b.content_type("up/pic.png").await.unwrap().as_deref(),
        Some("image/png")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn op_blob_upload_url_local_backend_errors_with_direct_put_hint() {
    let root = tmp_root();
    let local = LocalBlob::new(&root, "/v1/api").unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            blobs: Some(registry_with_default(Arc::new(local))),
            ..Default::default()
        },
    );
    // 缺省 opts = {"kind":"put"}；local 后端无预签名 → 错误文案指路直传 PUT 路由。
    let cap = b
        .run_with(
            r#"
                (async () => {
                    try {
                        await blob.uploadUrl("a/big.bin");
                        json.ok({ err: "" });
                    } catch (e) {
                        json.ok({ err: String(e) });
                    }
                })().catch((e) => json.fail(500, String(e)));
                "#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let err = v["data"]["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("local blob backend has no upload presign")
            && err.contains("direct PUT upload route"),
        "{err}"
    );
}

/// ABI 11 复制（local）：字节不进 V8（`fs::copy`），**src 保留**，ct 随行。
#[tokio::test(flavor = "current_thread")]
async fn local_copy_keeps_src_and_carries_content_type() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    b.put("tmp/a.bin", b"PAYLOAD", Some("application/x-foo"))
        .await
        .unwrap();
    b.copy("tmp/a.bin", "final/a.bin").await.unwrap();
    assert_eq!(b.get("final/a.bin").await.unwrap(), b"PAYLOAD".to_vec());
    // src 保留（copy 语义）
    assert_eq!(b.get("tmp/a.bin").await.unwrap(), b"PAYLOAD".to_vec());
    // ct sidecar 随行
    assert_eq!(
        b.content_type("final/a.bin").await.unwrap().as_deref(),
        Some("application/x-foo")
    );
    // 穿越拒绝（src / dst 双侧）
    for (s, d) in [
        ("../up", "ok"),
        ("ok", "../up"),
        ("a\\b", "ok"),
        ("ok", "a\\b"),
    ] {
        assert!(b.copy(s, d).await.is_err(), "{s} -> {d}");
    }
    // src 不存在 → 报错（不是静默建空对象）
    assert!(b.copy("nope", "final/nope").await.is_err());
    assert!(b.get("final/nope").await.is_err());
}

/// ABI 11 搬移（local）：src 消失；同目录走 rename、跨目录 copy + unlink。
#[tokio::test(flavor = "current_thread")]
async fn local_move_removes_src_same_and_cross_dir() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    b.put("tmp/same.bin", b"SAME", Some("application/x-foo"))
        .await
        .unwrap();
    b.put("tmp/cross.bin", b"CROSS", None).await.unwrap();
    // 同目录（rename）
    b.move_to("tmp/same.bin", "tmp/renamed.bin").await.unwrap();
    assert_eq!(b.get("tmp/renamed.bin").await.unwrap(), b"SAME".to_vec());
    assert!(b.get("tmp/same.bin").await.is_err(), "src 必须消失");
    // 跨目录（copy + unlink；dst 父目录不存在也要建成）
    b.move_to("tmp/cross.bin", "deep/er/cross.bin")
        .await
        .unwrap();
    assert_eq!(b.get("deep/er/cross.bin").await.unwrap(), b"CROSS".to_vec());
    assert!(b.get("tmp/cross.bin").await.is_err());
    // src 的 ct sidecar 一并清掉
    assert_eq!(
        b.content_type("tmp/renamed.bin").await.unwrap().as_deref(),
        Some("application/x-foo")
    );
    // 穿越拒绝
    assert!(b.move_to("tmp/renamed.bin", "../up").await.is_err());
    assert!(b.get("tmp/renamed.bin").await.is_ok(), "失败不能吃 src");
}

/// `src == dst` 是 no-op——`fs::copy` 同文件语义未定义（会截断），必须显式挡掉。
#[tokio::test(flavor = "current_thread")]
async fn local_copy_and_move_are_noop_when_src_equals_dst() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    b.put("a.bin", b"KEEPME", None).await.unwrap();
    b.copy("a.bin", "a.bin").await.unwrap();
    assert_eq!(b.get("a.bin").await.unwrap(), b"KEEPME".to_vec());
    b.move_to("a.bin", "a.bin").await.unwrap();
    assert_eq!(b.get("a.bin").await.unwrap(), b"KEEPME".to_vec());
}

/// ABI 11 区间读：短读截断、offset 过尾返回空、key 不存在报错、非法 key 拒绝。
#[tokio::test(flavor = "current_thread")]
async fn local_read_range_truncates_and_guards_keys() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    b.put("d/blob", b"blobdata", None).await.unwrap();
    assert_eq!(
        b.read_range("d/blob", 0, 4).await.unwrap(),
        b"blob".to_vec()
    );
    assert_eq!(b.read_range("d/blob", 2, 3).await.unwrap(), b"obd".to_vec());
    // 越尾截断（只要 4 字节，只剩 2 字节）
    assert_eq!(b.read_range("d/blob", 6, 4).await.unwrap(), b"ta".to_vec());
    // offset 过尾 / len = 0 → 空
    assert_eq!(b.read_range("d/blob", 99, 4).await.unwrap(), b"".to_vec());
    assert_eq!(b.read_range("d/blob", 0, 0).await.unwrap(), b"".to_vec());
    assert_eq!(b.read_range("d/blob", 8, 1).await.unwrap(), b"".to_vec());
    // key 不存在 / 非法 key
    assert!(b.read_range("nope", 0, 4).await.is_err());
    assert!(b.read_range("../up", 0, 4).await.is_err());
    assert!(b.read_range("a\\b", 0, 4).await.is_err());
    // 越界 offset + 巨量 len 不能溢出 panic
    assert_eq!(
        b.read_range("d/blob", u64::MAX - 1, u64::MAX)
            .await
            .unwrap(),
        b"".to_vec()
    );
}

/// key 含空格 / 非 ASCII：`fs_path` 必须与 `put`/`get` 落盘位置一致（ABI 11 的三条
/// 路径都在文件系统层自己算路径，编码一分叉就打到别的文件上）。
#[tokio::test(flavor = "current_thread")]
async fn local_copy_move_read_range_with_non_ascii_and_space_keys() {
    let root = tmp_root();
    let b = LocalBlob::new(&root, "/v1/api").unwrap();
    let src = "tmp/报 告 v2.bin";
    b.put(src, b"BYTES", None).await.unwrap();
    b.copy(src, "final/报 告 v2.bin").await.unwrap();
    assert_eq!(
        b.get("final/报 告 v2.bin").await.unwrap(),
        b"BYTES".to_vec()
    );
    assert_eq!(b.read_range(src, 1, 2).await.unwrap(), b"YT".to_vec());
    b.move_to(src, "final/moved.bin").await.unwrap();
    assert_eq!(b.get("final/moved.bin").await.unwrap(), b"BYTES".to_vec());
    assert!(b.get(src).await.is_err());
}

/// JS 面：`blob.copy` / `blob.move` / `blob.readRange`（含非整数 offset 拒绝）。
#[tokio::test(flavor = "current_thread")]
async fn op_blob_copy_move_read_range_via_bridge() {
    let root = tmp_root();
    let local = LocalBlob::new(&root, "/v1/api").unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            blobs: Some(registry_with_default(Arc::new(local))),
            ..Default::default()
        },
    );
    let cap = b
            .run_with(
                r#"
                (async () => {
                    await blob.put("tmp/a.bin", new Uint8Array([1,2,3,4,5,6,7,8]), "application/x-bin");
                    await blob.copy("tmp/a.bin", "final/a.bin");
                    await blob.move("tmp/a.bin", "final/b.bin");
                    let srcGone = false;
                    try { await blob.get("tmp/a.bin"); } catch (e) { srcGone = true; }
                    const copied = Array.from(await blob.readRange("final/a.bin", 2, 3)).join(",");
                    const moved = Array.from(await blob.get("final/b.bin")).join(",");
                    let badOffset = "";
                    try { await blob.readRange("final/a.bin", 1.5, 4); } catch (e) { badOffset = String(e); }
                    let negative = "";
                    try { await blob.readRange("final/a.bin", -1, 4); } catch (e) { negative = String(e); }
                    json.ok({
                        copied, moved, srcGone,
                        ct: await blob.contentType("final/b.bin"),
                        badOffset, negative,
                    });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["copied"], "3,4,5", "{v}");
    assert_eq!(v["data"]["moved"], "1,2,3,4,5,6,7,8", "{v}");
    assert_eq!(v["data"]["srcGone"], true, "{v}");
    assert_eq!(v["data"]["ct"], "application/x-bin", "{v}");
    assert!(
        v["data"]["badOffset"]
            .as_str()
            .unwrap()
            .contains("offset must be a non-negative integer"),
        "{v}"
    );
    assert!(
        v["data"]["negative"]
            .as_str()
            .unwrap()
            .contains("offset must be a non-negative integer"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn op_blob_content_type_via_bridge() {
    let root = tmp_root();
    let local = LocalBlob::new(&root, "/v1/api").unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            blobs: Some(registry_with_default(Arc::new(local))),
            ..Default::default()
        },
    );
    let cap = b
            .run_with(
                r#"
                (async () => {
                    await blob.put("js/a.bin", new Uint8Array([1]), "application/x-foo");
                    const explicit = await blob.contentType("js/a.bin");     // sidecar
                    const inferred = await blob.contentType("js/b.png");    // 未见过的 key → 扩展名推断
                    const missing = await blob.contentType("js/nope");      // 无 sidecar / 无扩展名 → null
                    json.ok({ explicit, inferred, missing });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["explicit"], "application/x-foo");
    assert_eq!(v["data"]["inferred"], "image/png");
    assert_eq!(v["data"]["missing"], "");
}
