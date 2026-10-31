use super::*;
use oj_plugin_ffi::{RBytes, RString};

fn addr(s: &str) -> Address {
    s.parse().expect("测试地址")
}

fn envelope() -> Envelope {
    Envelope::new(Some(addr("from@example.com")), vec![addr("to@example.com")]).expect("信封")
}

/// vtable 侧的附件字节（宿主解析结果）。
fn att(filename: &str, mime: &str, bytes: &[u8]) -> MailAttachment {
    MailAttachment {
        filename: RString::from(filename),
        mime: RString::from(mime),
        bytes: RBytes::from(bytes),
    }
}

/// 请求侧的附件引用（按 `blobKey` 声明，宿主据此解析）。
fn att_ref(filename: &str) -> AttachmentRef {
    AttachmentRef {
        filename: filename.to_string(),
        mime: None,
        blob_key: Some("k1".to_string()),
        path: None,
    }
}

fn req_with(
    text: Option<&str>,
    html: Option<&str>,
    attachments: Vec<AttachmentRef>,
) -> SendRequest {
    SendRequest {
        from: "from@example.com".to_string(),
        to: vec!["to@example.com".to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "主题".to_string(),
        text: text.map(str::to_string),
        html: html.map(str::to_string),
        headers: HashMap::new(),
        attachments,
        raw: None,
    }
}

// ---- Task 5.1：结构化组装 ----

#[test]
fn builds_multipart_alternative_with_html_and_attachments() {
    let req = req_with(Some("hi"), Some("<b>hi</b>"), vec![att_ref("a.pdf")]);
    let atts = vec![att("a.pdf", "application/pdf", b"%PDF-1.4")];
    let m = build_message(&req, &atts).expect("组装");
    let s = String::from_utf8(m.formatted()).expect("UTF-8");
    assert!(
        s.contains("multipart/alternative"),
        "text+html 应为备选体: {s}"
    );
    assert!(
        s.contains("a.pdf"),
        "附件名应出现在 Content-Disposition: {s}"
    );
    assert!(
        s.contains("application/pdf"),
        "附件类型应出现在 Content-Type: {s}"
    );
    assert!(
        !s.contains("JVBERi0xLjQ"),
        "附件字节不得被 base64 化成文本流（%PDF-1.4 的 base64）: {s}"
    );
    assert!(
        s.contains("hi") && s.contains("<b>hi</b>"),
        "两版正文都在: {s}"
    );
}

#[test]
fn rejects_attachment_count_mismatch() {
    let req = req_with(Some("hi"), None, vec![att_ref("a.pdf"), att_ref("b.pdf")]);
    let e = build_message(&req, &[]).expect_err("声明 2 个、宿主给 0 个必须 Err");
    assert!(
        e.contains('2') && e.contains('0'),
        "错误须点明两个数量: {e}"
    );
}

#[test]
fn rejects_attachment_ref_without_source_or_with_both() {
    let mut req = req_with(Some("hi"), None, vec![att_ref("a.pdf")]);
    req.attachments[0].blob_key = None; // 既无 blobKey 也无 path
    assert!(build_message(&req, &[att("a.pdf", "application/pdf", b"x")]).is_err());

    let mut req = req_with(Some("hi"), None, vec![att_ref("a.pdf")]);
    req.attachments[0].path = Some("a.pdf".to_string()); // 两个来源同时给
    let e = build_message(&req, &[att("a.pdf", "application/pdf", b"x")]).unwrap_err();
    assert!(
        e.contains("blobKey") && e.contains("path"),
        "错误须点明二选一: {e}"
    );
}

#[test]
fn text_only_and_html_only_are_single_part() {
    let text = build_message(&req_with(Some("plain"), None, vec![]), &[]).expect("text 组装");
    let s = String::from_utf8(text.formatted()).expect("UTF-8");
    assert!(s.contains("text/plain") && s.contains("plain"), "{s}");
    assert!(
        !s.contains("multipart/"),
        "单段正文不该被包成 multipart: {s}"
    );

    let html = build_message(&req_with(None, Some("<i>h</i>"), vec![]), &[]).expect("html 组装");
    let s = String::from_utf8(html.formatted()).expect("UTF-8");
    assert!(s.contains("text/html") && s.contains("<i>h</i>"), "{s}");
}

#[test]
fn cc_and_bcc_reach_envelope_but_bcc_header_is_dropped() {
    let mut req = req_with(Some("hi"), None, vec![]);
    req.cc = vec!["cc@example.com".to_string()];
    req.bcc = vec!["bcc@example.com".to_string()];
    let m = build_message(&req, &[]).expect("组装");
    let env: Vec<String> = m.envelope().to().iter().map(Address::to_string).collect();
    assert!(env.contains(&"cc@example.com".to_string()), "{env:?}");
    assert!(
        env.contains(&"bcc@example.com".to_string()),
        "Bcc 必须进 RCPT TO: {env:?}"
    );
    let s = String::from_utf8(m.formatted()).expect("UTF-8");
    assert!(s.contains("Cc: cc@example.com"), "{s}");
    assert!(
        !s.contains("bcc@example.com"),
        "Bcc 报头必须丢弃（收件人互不可见）: {s}"
    );
}

#[test]
fn rejects_illegal_address_naming_the_field() {
    let mut req = req_with(Some("hi"), None, vec![]);
    req.to = vec!["to@example.com".to_string(), "nope".to_string()];
    let e = build_message(&req, &[]).expect_err("非法 to 必须 Err");
    assert!(e.contains("to[1]"), "错误须点明是哪个字段: {e}");

    let mut req = req_with(Some("hi"), None, vec![]);
    req.from = "nope".to_string();
    let e = build_message(&req, &[]).expect_err("非法 from 必须 Err");
    assert!(e.contains("from"), "{e}");
}

#[test]
fn rejects_no_recipient_and_no_body() {
    let mut req = req_with(Some("hi"), None, vec![]);
    req.to.clear();
    assert!(build_message(&req, &[]).is_err(), "无收件人必须 Err");

    let e = build_message(&req_with(None, None, vec![]), &[]).expect_err("无正文必须 Err");
    assert!(
        e.contains("text") && e.contains("html"),
        "错误须给出下一步: {e}"
    );
}

#[test]
fn headers_are_set_but_structured_fields_cannot_be_overridden() {
    let mut req = req_with(Some("hi"), None, vec![]);
    req.headers.insert("X-Trace".to_string(), "t-1".to_string());
    let m = build_message(&req, &[]).expect("组装");
    let s = String::from_utf8(m.formatted()).expect("UTF-8");
    assert!(s.contains("X-Trace: t-1"), "自定义头应写入: {s}");
    assert!(s.contains("Subject: "), "结构化 subject 应写入: {s}");

    for name in ["bcc", "Bcc", "FROM", "to", "Subject"] {
        let mut req = req_with(Some("hi"), None, vec![]);
        req.headers
            .insert(name.to_string(), "victim@example.com".to_string());
        if let Ok(m) = build_message(&req, &[]) {
            let s = String::from_utf8(m.formatted()).unwrap_or_default();
            panic!("headers 覆盖 {name} 必须 Err（否则可绕过收件人白名单），实际组装出：{s}");
        }
    }
}

#[test]
fn rejects_crlf_in_subject_headers_and_addresses() {
    let mut req = req_with(Some("hi"), None, vec![]);
    req.subject = "s\r\nBcc: victim@example.com".to_string();
    let e = build_message(&req, &[]).expect_err("subject 含 CRLF 必须 Err");
    assert!(e.contains("CR/LF"), "错误须点明头注入: {e}");

    let mut req = req_with(Some("hi"), None, vec![]);
    req.headers.insert(
        "X-A".to_string(),
        "v\r\nBcc: victim@example.com".to_string(),
    );
    let e = build_message(&req, &[]).expect_err("headers 值含 CRLF 必须 Err");
    assert!(e.contains("CR/LF"), "错误须点明头注入: {e}");

    let mut req = req_with(Some("hi"), None, vec![]);
    req.headers
        .insert("X-A\r\nBcc".to_string(), "v".to_string());
    assert!(build_message(&req, &[]).is_err(), "头名含 CRLF 必须 Err");
}

#[test]
fn attachment_mime_falls_back_to_request_then_default() {
    let mut req = req_with(Some("hi"), None, vec![att_ref("a.bin")]);
    req.attachments[0].mime = Some("image/png".to_string());
    // 宿主没解析出 mime（空串）→ 回落请求里显式给的
    let m = build_message(&req, &[att("a.bin", "", b"x")]).expect("组装");
    let s = String::from_utf8(m.formatted()).expect("UTF-8");
    assert!(s.contains("image/png"), "{s}");

    // 两处都没有 → 默认二进制流
    let m = build_message(
        &req_with(Some("hi"), None, vec![att_ref("a.bin")]),
        &[att("a.bin", "", b"x")],
    )
    .expect("组装");
    let s = String::from_utf8(m.formatted()).expect("UTF-8");
    assert!(s.contains(DEFAULT_MIME), "{s}");
}

#[test]
fn deserializes_blob_key_and_defaults() {
    let r: SendRequest = serde_json::from_str(
            r#"{"from":"f@x.com","to":["t@x.com"],"raw":"x","attachments":[{"filename":"a.pdf","blobKey":"k1"}]}"#,
        )
        .expect("反序列化");
    assert_eq!(r.attachments[0].blob_key.as_deref(), Some("k1"));
    assert!(
        r.subject.is_empty(),
        "subject 缺省为空串（raw 路不需要主题）"
    );
    assert!(r.cc.is_empty() && r.bcc.is_empty() && r.headers.is_empty());
    assert!(r.text.is_none() && r.html.is_none());
}

#[test]
fn envelope_of_validates_from_and_to() {
    assert!(
        envelope_of("from@example.com", &[]).is_err(),
        "空 to 必须 Err"
    );
    assert!(envelope_of("nope", &["t@example.com".to_string()]).is_err());
    assert!(envelope_of("from@example.com", &["nope".to_string()]).is_err());
    let e = envelope_of("from@example.com", &["t@example.com".to_string()]).expect("信封");
    assert_eq!(e.to().len(), 1);
    assert!(e.from().is_some());
}

/// 地址口径**对账**（阶段 7 遗留 §8.2）：宿主 `mail.rs::validate_address` 放行的地址，
/// 插件不得拒绝——否则出现「宿主过了、投递却 `code:5`」的口径分裂。
///
/// **实测（阶段 8 新增，TDD 首跑即 RED）**：raw 路与宿主**逐条一致**（两侧同为
/// `lettre::Address`）；结构化路用的 `lettre::Mailbox` 与 `Address` 的接受集**并不互相包含**：
/// - `Address` 收 / `Mailbox` 拒 → `"quoted local"@x.com`、域字面量 `user@[127.0.0.1]`
///   ——即「宿主放行、插件拒绝」的**已知分裂**（方向为插件更严：信封派生的地址必然先过
///   宿主白名单，故**无越权面**，只是对用户表现为一个没道理的 `code:5`）；
/// - `Mailbox` 收 / `Address` 拒 → `显示名 <user@x.com>`——宿主是权威前置门，先拒，安全。
///
/// 处置（阶段 8 硬约束：不改实现语义）：**登记为已知偏差**（见阶段 8 小结「已知偏差 D3」），
/// 本用例把两侧实测边界**钉死**（分裂项列表化，不无限容忍），任何漂移都会被抓住。
#[test]
fn host_accepted_addresses_are_accepted_by_plugin_or_listed_as_known_split() {
    let corpus = [
        "user@example.com",
        "user.name+tag@example.com",
        "UPPER@Example.COM",
        "user@localhost",
        "\"quoted local\"@example.com",
        "user@[127.0.0.1]",
        "显示名 <user@example.com>", // Address 拒、Mailbox 收（宿主先拒，方向安全）
        "nope",
        "a@x.com\r\nBcc: evil@y.com",
        "",
    ];
    /// `Address` 收 / `Mailbox` 拒的**已知分裂**（D3；修好后应从本表移除并放宽断言）。
    const KNOWN_SPLIT: [&str; 2] = ["\"quoted local\"@example.com", "user@[127.0.0.1]"];

    let mut host_accepted = 0usize;
    for s in corpus {
        let host_ok = s.parse::<Address>().is_ok();
        // raw 路与宿主同一解析器：放行/拒绝必须逐条一致（口径同源的结构性保证）。
        assert_eq!(
            host_ok,
            envelope_of("from@example.com", &[s.to_string()]).is_ok(),
            "raw 路必须与宿主同判：{s:?}"
        );
        if !host_ok {
            continue;
        }
        host_accepted += 1;
        let mut req = req_with(Some("hi"), None, vec![]);
        req.from = s.to_string();
        req.to = vec![s.to_string()];
        let structured_ok = build_message(&req, &[]).is_ok();
        if KNOWN_SPLIT.contains(&s) {
            assert!(
                !structured_ok,
                "{s:?} 的已知分裂（D3）行为变了——若已修好，请从 KNOWN_SPLIT 移除并更新阶段 8 小结"
            );
        } else {
            assert!(
                structured_ok,
                "宿主放行的地址 {s:?} 不得被结构化路（Mailbox）拒绝"
            );
        }
    }
    assert!(host_accepted >= 3, "语料须含至少 3 个宿主放行样本");

    // 反向：宿主拒绝的形态，raw 路（与宿主同解析器）必须同样拒绝。
    for bad in ["nope", "a@b@c", "a b@x.com", "a@x.com\r\nBcc: evil@y.com"] {
        assert!(bad.parse::<Address>().is_err(), "{bad} 应被宿主拒");
        assert!(
            envelope_of("from@example.com", &[bad.to_string()]).is_err(),
            "raw 路必须与宿主同判：{bad}"
        );
    }

    // CRLF 是**拒绝**口径（不是剥离）：即便 `Mailbox` 自己会忽略尾部 `\r\n`，
    // `mailbox()` 的前置 `ensure_no_crlf` 也必须先拒——纵深防御不得只靠解析器。
    for bad in ["a@x.com\r\n", "a@x.com\n", "a@x.com\r\nBcc: evil@y.com"] {
        assert!(
            mailbox(bad, "to[0]").is_err(),
            "结构化路必须先拒 CRLF 地址（不依赖 Mailbox 的宽容解析）：{bad:?}"
        );
    }
}

// ---- Task 5.2：raw 信封头剥离（controller 决策 2026-09-15：**只剥信封头**，Subject 保留）----

/// raw 路的剥离范围 = **信封头** `From`/`To`/`Cc`/`Bcc`（大小写不敏感）；`Subject` 不是信封
/// 字段，**保留**（剥它只会让邮件丢主题），其折行续行同样保留；其余头与正文不动。
#[test]
fn raw_strips_envelope_headers_but_keeps_subject_and_body() {
    let raw = "FROM: evil@x\nTo: victim@x\nCc: c@x\nBCC: b@x\nSUBJECT: raw-sub\n  folded\nX-Keep: 1\n\nbody";
    let m = build_raw(&envelope(), raw, "", &[]).expect("剥离");
    let s = String::from_utf8(m).expect("UTF-8");
    let low = s.to_lowercase();
    assert!(
        low.contains("subject: raw-sub") && s.contains("folded"),
        "原文 Subject（含折行续行）必须保留: {s}"
    );
    assert_eq!(
        low.matches("subject:").count(),
        1,
        "结构化为空时不得凭空多出 Subject 头: {s}"
    );
    assert!(
        !s.contains("evil@x") && !s.contains("victim@x"),
        "原文信封头已剥离: {s}"
    );
    assert!(
        !s.contains("c@x") && !s.contains("b@x"),
        "原文 Cc/Bcc 已剥离: {s}"
    );
    assert!(
        s.contains("X-Keep: 1") && s.contains("body"),
        "其余头与正文保留: {s}"
    );
    assert!(
        s.contains("From: from@example.com") && s.contains("To: to@example.com"),
        "报头由结构化信封重建: {s}"
    );
}

/// 原文 `Subject` 里的裸 CR（行扫描不会把它当行界，但它是头注入面）必须按既有 CRLF 口径拒绝。
#[test]
fn raw_rejects_crlf_injection_in_raw_subject() {
    let raw = "Subject: a\rBcc: victim@x\n\nbody";
    let e = build_raw(&envelope(), raw, "", &[]).expect_err("原文 Subject 含 CR 必须 Err");
    assert!(e.contains("CR/LF"), "错误须点明头注入: {e}");
}

/// 结构化 `subject` 非空 ⇒ **覆盖**原文 Subject：最终只有一个 Subject 头，值 = 结构化值，
/// 原文 Subject 行连同其折行续行整行丢弃。
#[test]
fn raw_structured_subject_overrides_raw_subject() {
    let raw = "Subject: raw-sub\n  folded-leak\nX-Keep: 1\n\nbody";
    let s = String::from_utf8(build_raw(&envelope(), raw, "struct-sub", &[]).expect("组装"))
        .expect("UTF-8");
    let low = s.to_lowercase();
    assert!(
        s.contains("Subject: struct-sub"),
        "结构化 subject 生效: {s}"
    );
    assert!(
        !s.contains("raw-sub") && !s.contains("folded-leak"),
        "原文 Subject 及其折行续行必须整段丢弃: {s}"
    );
    assert_eq!(
        low.matches("subject:").count(),
        1,
        "最终只能有一个 Subject 头: {s}"
    );
    assert!(s.contains("X-Keep: 1") && s.contains("body"), "{s}");
}

/// 结构化 `subject` 含 CR/LF 同样拒绝（头注入纵深防线）；非 ASCII 值由 lettre 做 RFC2047 编码。
#[test]
fn raw_rejects_crlf_in_structured_subject_and_encodes_non_ascii() {
    let e = build_raw(&envelope(), "X-Keep: 1\n\nbody", "s\r\nBcc: victim@x", &[])
        .expect_err("结构化 subject 含 CRLF 必须 Err");
    assert!(e.contains("CR/LF"), "{e}");

    let s = String::from_utf8(
        build_raw(&envelope(), "X-Keep: 1\n\nbody", "结构主题", &[]).expect("组装"),
    )
    .expect("UTF-8");
    assert!(
        s.contains("Subject: =?utf-8?") && !s.contains("结构主题"),
        "非 ASCII 主题应经 RFC2047 编码: {s}"
    );
}

#[test]
fn raw_strips_folded_continuation_of_stripped_header_only() {
    let raw = "X-Fold: a\n  keep-me\nFrom: evil@x\n\tleak-me\nX-Two: 2\n\nbody";
    let s = String::from_utf8(build_raw(&envelope(), raw, "", &[]).expect("剥离")).expect("UTF-8");
    assert!(
        s.contains("X-Fold: a") && s.contains("keep-me"),
        "保留头的折行保留: {s}"
    );
    assert!(!s.contains("leak-me"), "被剥离头的折行不得残留: {s}");
    assert!(s.contains("X-Two: 2"), "{s}");
}

#[test]
fn raw_normalizes_lone_lf_to_crlf_and_keeps_body_otherwise_verbatim() {
    let raw = "X-A: 1\n\nline1\n.\nline2";
    let s = String::from_utf8(build_raw(&envelope(), raw, "", &[]).expect("剥离")).expect("UTF-8");
    assert!(s.contains("X-A: 1\r\n"), "头行归一为 CRLF: {s}");
    assert!(
        !s.contains("\n.\n"),
        "裸 LF 后的行首点号是 SMTP smuggling 面（dot-stuffing 需要 CRLF）: {s}"
    );
    assert!(s.contains("line1\r\n.\r\nline2"), "正文除行尾外不改: {s}");
    assert!(s.ends_with("line2"), "正文尾部不多不少: {s}");
}

#[test]
fn raw_rejects_attachments_and_missing_sender() {
    let e = build_raw(
        &envelope(),
        "X-A: 1\n\nb",
        "",
        &[att("a.pdf", "application/pdf", b"x")],
    )
    .expect_err("raw 与附件互斥");
    assert!(e.contains("附件"), "{e}");

    let senderless = Envelope::new(None, vec![addr("to@example.com")]).expect("信封");
    assert!(
        build_raw(&senderless, "X-A: 1\n\nb", "", &[]).is_err(),
        "缺发件人必须 Err"
    );
}

#[test]
fn raw_without_blank_line_keeps_headers_and_empty_body() {
    let s =
        String::from_utf8(build_raw(&envelope(), "X-A: 1", "", &[]).expect("剥离")).expect("UTF-8");
    assert!(
        s.contains("X-A: 1\r\n\r\n"),
        "无空行时按「全是头、空正文」处理: {s}"
    );
}

/// B4（SMTP smuggling）：正文里的**裸 CR** 必须一并归一到 CRLF。
///
/// 攻击形态：正文含 `X\r.\r\n` —— 若接收端/中间 MTA 以裸 CR 为行界，DATA 在这里就
/// 提前结束（那行 `.` 未被点填充），余下内容被当 SMTP 命令执行（可伪造 MAIL FROM /
/// RCPT TO）。归一后 `X\r.\r\n` → `X\r\n.\r\n`：那条 `.` 成了正常行首点，由 lettre 的
/// 点填充正确转义。
#[test]
fn raw_normalizes_bare_cr_in_body_to_crlf() {
    // 正文（不是头区）：`\r` 归一，`\r\n` 不动。
    let s =
        String::from_utf8(build_raw(&envelope(), "X-A: 1\n\nX\r.\r\ntail", "", &[]).expect("组装"))
            .expect("UTF-8");
    assert!(
        !s.contains("X\r."),
        "正文里的裸 CR 必须被中和（否则 DATA 可提前结束）: {s:?}"
    );
    assert!(
        s.contains("X\r\n.\r\ntail"),
        "裸 CR 应归一为 CRLF（`\\r.` → `\\r\\n.`）: {s:?}"
    );
    assert!(!s.contains('\r') || !s.contains("\r\r"), "{s:?}");

    // 三种行尾（裸 CR / 裸 LF / CRLF）归一后都只有一个 CRLF。
    assert_eq!(normalize_crlf("a\rb"), "a\r\nb");
    assert_eq!(normalize_crlf("a\nb"), "a\r\nb");
    assert_eq!(normalize_crlf("a\r\nb"), "a\r\nb");
    assert_eq!(normalize_crlf("a\r\r\nb"), "a\r\n\r\nb");
    assert_eq!(normalize_crlf("a\r\n\r\nb"), "a\r\n\r\nb", "幂等");
}

/// B4：**头区**含裸 CR 一律**拒绝**（而非归一）—— 归一它等于凭空造出一个头
/// （`X-Foo: a\rBcc: x` 会变成两条头），且静默改写调用方的头比报错更糟。
#[test]
fn raw_rejects_bare_cr_in_kept_header() {
    for raw in [
        "X-Foo: a\rBcc: victim@x\n\nbody",   // 普通头
        "X-Foo: a\n \rfolded\n\nbody",       // 折行续行
        "Subject: a\rBcc: victim@x\n\nbody", // Subject（结构化 subject 为空 → 保留）
    ] {
        let e = build_raw(&envelope(), raw, "", &[]).expect_err("头区裸 CR 必须 Err");
        assert!(e.contains("CR/LF") && e.contains("下一步"), "{raw:?} → {e}");
    }
    // 被剥离的头里的裸 CR 无所谓（整行丢弃）：不得因此报错。
    let s = String::from_utf8(build_raw(&envelope(), "From: a\rX\n\nbody", "", &[]).expect("剥离"))
        .expect("UTF-8");
    assert!(!s.contains("From: a\rX"), "被剥离头不得残留: {s:?}");
}

/// B4：结构化 `text`/`html` 正文里的裸 CR 同样归一（组装路是另一条写正文的路径）。
#[test]
fn structured_bodies_normalize_bare_cr() {
    let req = req_with(Some("X\r.\r\ntail"), Some("<b>a\rb</b>"), vec![]);
    let s = String::from_utf8(build_message(&req, &[]).expect("组装").formatted()).expect("UTF-8");
    assert!(!s.contains("X\r."), "text 里的裸 CR 必须中和: {s:?}");
    // HTML 段（base64/quoted-printable 之外的形态不做字节级断言）：
    // 直接钉归一函数的语义即可。
    assert_eq!(normalize_crlf("<b>a\rb</b>"), "<b>a\r\nb</b>");
}

/// B5：`headers` 不得覆盖**弱 spoof 面**头（`Sender`/`Return-Path`/`Reply-To`）——
/// `Sender` 断言实际提交者、`Return-Path` 是退回地址（客户端本就不该发）、
/// `Reply-To` 能把回复引到别处。三者与既有 From/To/Cc/Bcc/Subject 同列禁用。
#[test]
fn headers_cannot_override_spoof_prone_headers() {
    for name in [
        "Sender",
        "sender",
        "SENDER",
        "Return-Path",
        "return-path",
        "Reply-To",
        "reply-to",
    ] {
        let mut req = req_with(Some("hi"), None, vec![]);
        req.headers
            .insert(name.to_string(), "attacker@evil.com".to_string());
        let e = build_message(&req, &[]).expect_err("弱 spoof 面头必须禁覆盖");
        assert!(
            e.contains("不允许覆盖") && e.contains(name),
            "错误须点名头名 {name}: {e}"
        );
    }
}

/// B5：raw 路的剥离清单补 `Sender`/`Return-Path`（含折行续行）—— 二者与结构化信封
/// 重建的 `From` 矛盾即弱 spoof；`Reply-To` **不剥**（raw 是调用方自备原文，它不改变
/// 信封与发件人身份）。
#[test]
fn raw_strips_sender_and_return_path_but_keeps_reply_to() {
    let raw = "Sender: evil@x\n  leak\nReturn-Path: <evil@x>\nReply-To: r@x\nX-Keep: 1\n\nbody";
    let s = String::from_utf8(build_raw(&envelope(), raw, "", &[]).expect("剥离")).expect("UTF-8");
    let low = s.to_lowercase();
    assert!(!low.contains("sender:") && !low.contains("leak"), "{s}");
    assert!(!low.contains("return-path") && !s.contains("evil@x"), "{s}");
    assert!(s.contains("Reply-To: r@x"), "Reply-To 保留: {s}");
    assert!(s.contains("X-Keep: 1") && s.contains("body"), "{s}");
}
