//! 阶段 5：请求反序列化 + 消息组装。
//!
//! 两条互斥的投递路（`engine::submit` 按 `req.raw` 有无二选一）：
//!
//! - **结构化组装**（[`build_message`]）：`from`/`to`/`cc`/`bcc`/`subject`/`text`/`html`/
//!   `headers`/`attachments` → `lettre::Message`（text+html → `multipart/alternative`；
//!   有附件 → 外层 `multipart/mixed`）。信封（MAIL FROM / RCPT TO）由 lettre 从报头派生
//!   （To ∪ Cc ∪ Bcc；`Bcc` 报头在派生后按 lettre 默认丢弃 → 收件人可见性正确）。
//! - **原文投递**（[`build_raw`]）：`raw` 是调用方自备的 RFC5322 原文，**字节原样**送出，
//!   仅剥离冲突头（见下）。**不**经 lettre 的 MIME 组装 —— `Message::body` 会按「最优编码」
//!   重编码（行 ≥76 字节即改用 quoted-printable/base64），已编码的 multipart 原文会被改烂。
//!
//! ## 附件对齐契约（宿主 ↔ 插件）
//!
//! `SendRequest::attachments[i]` 是**引用**（`blobKey` / `path` 二选一，宿主解析成字节），
//! 宿主按**下标**把解析结果放进 vtable 的 `atts[i]`。故：
//!
//! | 字段 | 来源 | 说明 |
//! |---|---|---|
//! | 字节 | `atts[i].bytes` | 宿主解析后的**原始字节**（非 base64），插件直接喂 lettre |
//! | MIME | `atts[i].mime` | 宿主已按「显式 `mime` 优先，否则嗅探」填好；空则回落 `attachments[i].mime`，再空则 `application/octet-stream` |
//! | 文件名 | `attachments[i].filename` | 请求声明的名字；空则回落 `atts[i].filename` |
//!
//! **长度必须一致**：`attachments.len() != atts.len()` ⇒ `Err`（fail-loud）。下标错位无法
//! 从字节本身反推，静默错配会把 A 的字节挂到 B 的名字上 —— 宁可整封拒投。
//!
//! ## 头注入防线（纵深，主防线在宿主）
//!
//! 设计 §10/§11 要求宿主在过线前剥离 CRLF；插件作为 MIME 输出的最后一环**再拒一次**：
//! `subject`/`headers` 值/各地址含 `\r`/`\n` ⇒ `Err`（`code:5`）。`headers` 另**禁止**
//! 覆盖 `From`/`To`/`Cc`/`Bcc`/`Subject` —— 否则可绕过宿主的收件人白名单（信封由报头派生）。

use lettre::address::{Address, Envelope};
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use oj_plugin_ffi::MailAttachment;
use serde::Deserialize;
use std::collections::HashMap;

/// raw 路必须剥离的冲突头（**大小写不敏感**）：信封/主题的权威来源是结构化字段，
/// 原文里的这些头留着就能造出「双收件人 / 发件人 spoof / 主题错配」。
const CONFLICTING_HEADERS: [&str; 5] = ["from", "to", "cc", "bcc", "subject"];

/// 附件未给 MIME 且宿主未解析出时的兜底类型。
const DEFAULT_MIME: &str = "application/octet-stream";

/// 附件引用（**宿主**据此取字节；插件只用 `filename`/`mime`）。
#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentRef {
    /// 请求声明的文件名（显示名，即收件人看到的 `filename=`）。
    pub filename: String,
    /// 显式 MIME；宿主侧「显式优先，否则按扩展名/字节嗅探」。
    #[serde(default)]
    pub mime: Option<String>,
    /// 宿主从 blob 存储取字节的键（与 `path` **二选一**）。
    #[serde(default, rename = "blobKey")]
    pub blob_key: Option<String>,
    /// 宿主从本地文件取字节的路径（与 `blobKey` **二选一**；越界校验在宿主）。
    #[serde(default)]
    pub path: Option<String>,
}

/// 一次投递的组装请求（vtable `submit` 的 `req` JSON 形态）。
///
/// `subject` 带 `#[serde(default)]`：`sendRaw` 只传 `from`/`to`/`raw`（design §5），
/// 要求它必须出现会把 raw 路直接堵死。
#[derive(Debug, Clone, Deserialize)]
pub struct SendRequest {
    /// 信封发件人（也是重建 `From:` 报头的唯一来源）。
    pub from: String,
    /// 信封收件人（至少一个）。
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    /// 主题（raw 路的主题见模块头：原文 `Subject:` 会被剥离）。
    #[serde(default)]
    pub subject: String,
    /// 纯文本正文。
    #[serde(default)]
    pub text: Option<String>,
    /// HTML 正文。
    #[serde(default)]
    pub html: Option<String>,
    /// 自定义报头（不得覆盖 `From`/`To`/`Cc`/`Bcc`/`Subject`）。
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// 附件引用表（与 vtable `atts` **按下标对齐**）。
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// RFC5322 原文。给定时走 [`build_raw`]（此时 `attachments` 必须为空）。
    #[serde(default)]
    pub raw: Option<String>,
}

/// 结构化 `from`/`to` → SMTP 信封（MAIL FROM / RCPT TO）。
///
/// raw 路也是**唯一**的信封来源（与原文里的 `To:`/`Cc:` 头解耦，防双收件人）。
pub fn envelope_of(from: &str, to: &[String]) -> Result<Envelope, String> {
    ensure_no_crlf(from, "from")?;
    if to.is_empty() {
        return Err("请求缺少 to（信封收件人）：请给出至少一个收件人".to_string());
    }
    let from: Address = from
        .parse()
        .map_err(|e| format!("from 地址非法: {e}（下一步：改为 user@domain 形式）"))?;
    let mut rcpt = Vec::with_capacity(to.len());
    for (i, t) in to.iter().enumerate() {
        ensure_no_crlf(t, &format!("to[{i}]"))?;
        rcpt.push(
            t.parse()
                .map_err(|e| format!("to[{i}] 地址非法: {e}（下一步：改为 user@domain 形式）"))?,
        );
    }
    Envelope::new(Some(from), rcpt).map_err(|e| format!("信封非法: {e}"))
}

/// 结构化组装：`SendRequest` + 宿主解析的附件字节 → [`Message`]。
///
/// 失败一律 `Err(原因 + 下一步)`（由 `engine` 映射为 `code:5`）。
pub fn build_message(req: &SendRequest, atts: &[MailAttachment]) -> Result<Message, String> {
    let atts = align_attachments(req, atts)?;
    if req.to.is_empty() {
        return Err(
            "缺少收件人：to 不能为空（下一步：给出至少一个 to；只抄送请同时给 to）".to_string(),
        );
    }

    let mut b = Message::builder().from(mailbox(&req.from, "from")?);
    for (i, t) in req.to.iter().enumerate() {
        b = b.to(mailbox(t, &format!("to[{i}]"))?);
    }
    for (i, c) in req.cc.iter().enumerate() {
        b = b.cc(mailbox(c, &format!("cc[{i}]"))?);
    }
    // Bcc 进 RCPT TO，但 lettre 默认在派生信封后丢弃 `Bcc` 报头（收件人互不可见）。
    for (i, c) in req.bcc.iter().enumerate() {
        b = b.bcc(mailbox(c, &format!("bcc[{i}]"))?);
    }
    if !req.subject.is_empty() {
        ensure_no_crlf(&req.subject, "subject")?;
        b = b.subject(req.subject.clone());
    }
    for (name, value) in &req.headers {
        b = b.raw_header(custom_header(name, value)?);
    }

    let content = match (req.text.as_deref(), req.html.as_deref()) {
        // 两版正文 → 备选体（收件人客户端自行择优）。
        (Some(t), Some(h)) => Some(Content::Multi(MultiPart::alternative_plain_html(
            t.to_string(),
            h.to_string(),
        ))),
        (Some(t), None) => Some(Content::Single(SinglePart::plain(t.to_string()))),
        (None, Some(h)) => Some(Content::Single(SinglePart::html(h.to_string()))),
        (None, None) => None,
    };

    let built = match (content, atts.is_empty()) {
        (None, true) => {
            return Err(
                "缺少正文：text 与 html 至少给一个（或带附件）（下一步：补 text/html）".to_string(),
            );
        }
        (Some(Content::Single(s)), true) => b.singlepart(s),
        (Some(Content::Multi(m)), true) => b.multipart(m),
        (content, false) => {
            // 有附件 → 外层 mixed：正文（单段/备选体）与各附件平级。
            let mut mixed = MultiPart::mixed().build();
            mixed = match content {
                Some(Content::Single(s)) => mixed.singlepart(s),
                Some(Content::Multi(m)) => mixed.multipart(m),
                None => mixed, // 只发附件（MIME 合法）
            };
            for a in &atts {
                mixed = mixed.singlepart(attachment_part(a)?);
            }
            b.multipart(mixed)
        }
    };
    built.map_err(|e| format!("消息组装失败: {e}（下一步：检查 from/to 与正文/附件是否完整）"))
}

/// 正文段（单段或备选体）；附件在外层 `mixed`，故与它正交。
enum Content {
    Single(SinglePart),
    Multi(MultiPart),
}

/// 一条**已对齐**的附件（下标相同 ⇒ 文件名/类型/字节同源，见模块头）。
struct AlignedAttachment<'a> {
    name: &'a str,
    mime: &'a str,
    bytes: &'a [u8],
}

/// 请求附件引用 ↔ 宿主解析字节的**下标对齐 + 一致性校验**（fail-loud，绝不静默错配）。
fn align_attachments<'a>(
    req: &'a SendRequest,
    atts: &'a [MailAttachment],
) -> Result<Vec<AlignedAttachment<'a>>, String> {
    if req.attachments.len() != atts.len() {
        return Err(format!(
            "附件数量不匹配：请求声明 {} 个（attachments），宿主解析出 {} 个（atts）——按下标对齐失败（下一步：逐个检查 attachments[i] 的 blobKey/path 是否存在且可读）",
            req.attachments.len(),
            atts.len()
        ));
    }
    let mut out = Vec::with_capacity(atts.len());
    for (i, (r, a)) in req.attachments.iter().zip(atts).enumerate() {
        // 引用必须且只能给一个来源（宿主按它取字节；两个都给 = 谁是权威不明确）。
        if r.blob_key.is_some() == r.path.is_some() {
            return Err(format!(
                "attachments[{i}] 必须且只能给 blobKey 或 path 之一（当前 blobKey={:?}, path={:?}）（下一步：二选一）",
                r.blob_key, r.path
            ));
        }
        let name = if r.filename.is_empty() {
            &a.filename[..]
        } else {
            &r.filename[..]
        };
        if name.is_empty() {
            return Err(format!(
                "attachments[{i}] 缺少 filename（请求与宿主都没给）（下一步：给出显示用的文件名）"
            ));
        }
        // MIME：宿主解析结果优先（「显式优先，否则嗅探」），再回落请求显式值，最后兜底。
        let mime = if !a.mime.is_empty() {
            &a.mime[..]
        } else {
            r.mime
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or(DEFAULT_MIME)
        };
        out.push(AlignedAttachment {
            name,
            mime,
            bytes: a.bytes.as_slice(),
        });
    }
    Ok(out)
}

/// 单个附件段：字节直接取自宿主（**原始字节**，非 base64；ASCII 内容走 7bit 原样送出）。
fn attachment_part(a: &AlignedAttachment<'_>) -> Result<SinglePart, String> {
    let ct = ContentType::parse(a.mime).map_err(|e| {
        format!(
            "附件 '{}' 的 mime 非法（{}）: {e}（下一步：改为 type/subtype）",
            a.name, a.mime
        )
    })?;
    Ok(Attachment::new(a.name.to_string()).body(a.bytes.to_vec(), ct))
}

/// `src` → `Mailbox`（强校验，含显示名写法 `姓名 <a@b>`）。
fn mailbox(src: &str, field: &str) -> Result<Mailbox, String> {
    ensure_no_crlf(src, field)?;
    src.parse::<Mailbox>()
        .map_err(|e| format!("{field} 地址非法: {e}（下一步：改为 user@domain 形式）"))
}

/// 自定义报头 → `HeaderValue`（lettre 负责 RFC2047 编码与折行）。
///
/// **禁止**覆盖 `From`/`To`/`Cc`/`Bcc`/`Subject`：信封与主题的权威来源是结构化字段，
/// 允许覆盖就能绕过宿主的收件人白名单（信封由报头派生）。
fn custom_header(name: &str, value: &str) -> Result<HeaderValue, String> {
    ensure_no_crlf(name, "headers 名称")?;
    ensure_no_crlf(value, "headers 值")?;
    if CONFLICTING_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
    {
        return Err(format!(
            "headers 不允许覆盖 {name}：From/To/Cc/Bcc/Subject 由结构化字段决定（下一步：改用 from/to/cc/bcc/subject，或换一个自定义头名）"
        ));
    }
    let n = HeaderName::new_from_ascii(name.to_string()).map_err(|_| {
        format!(
            "headers 名称非法（{name}）：须为非空 ASCII 且不含 ':'/' '（下一步：改用合法的头名）"
        )
    })?;
    Ok(HeaderValue::new(n, value.to_string()))
}

/// raw 路：剥离冲突头后的**最终 RFC5322 字节**（信封由调用方另行交给 transport）。
///
/// 与 [`build_message`] 不同，这里不经 lettre 组装：原文（除冲突头与行尾）逐字节保留。
/// 失败一律 `Err(原因 + 下一步)`（由 `engine` 映射为 `code:5`）。
///
/// ## 为什么不让 lettre 组装 raw
///
/// `MessageBuilder::body` 会按「最优编码」重编码正文（行 ≥76 字节就改 quoted-printable /
/// base64），而 `Content-Type`/`Content-Transfer-Encoding` 等原文报头是**原样保留**的 ——
/// 一个 base64 分块的 multipart 原文会被 `multipart/*` 报头 + base64 正文的组合改烂
/// （收件端解析不出任何 part）。故 raw 路自己拼字节。
///
/// ## 行尾一律归一 CRLF
///
/// SMTP DATA 的帧界是 CRLF，而 lettre 的送出侧只做**点填充**（`ClientCodec`）不做行尾
/// 归一：裸 LF 之后的行首 `.` 不会被填充，中间 MTA 可据此提前结束 DATA → 余下内容被当
/// 命令执行（SMTP smuggling）。故这里把头/正文的行尾统一成 CRLF；正文其余字节不动。
pub fn build_raw(
    envelope: &Envelope,
    raw: &str,
    atts: &[MailAttachment],
) -> Result<Vec<u8>, String> {
    // vtable 契约：raw 给定时宿主不解析附件（atts 必为空）。非空 = 调用方把两条路混用了。
    if !atts.is_empty() {
        return Err(
            "raw 与附件互斥：raw 给定时附件必须为空（vtable 契约：宿主不为 raw 解析附件）"
                .to_string(),
        );
    }
    let from = envelope
        .from()
        .ok_or_else(|| "raw 路缺少信封发件人（MAIL FROM）：请给出结构化 from".to_string())?;
    let (head, body) = split_head_body(raw);

    let mut out = String::with_capacity(raw.len() + 96);
    // 报头自结构化信封重建（原文的 From/To/Cc/Bcc/Subject 已剥离 → 不存在双收件人/spoof）。
    out.push_str(&format!("From: {from}\r\n"));
    let rcpt: Vec<String> = envelope.to().iter().map(Address::to_string).collect();
    out.push_str(&format!("To: {}\r\n", rcpt.join(", ")));
    for line in kept_header_lines(head) {
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n'); // 无空行的「全是头」原文：补上行尾，别把正文粘到最后一个头上
        }
    }
    out.push_str("\r\n"); // 头/正文分隔空行
    out.push_str(body);
    Ok(normalize_crlf(&out).into_bytes())
}

/// 头部区中**保留**的行（含原行尾）：剥离冲突头，其余头（及其折行续行）原样保留。
///
/// 折行（continuation，行首为空格/TAB）归属**上一个头**：上一个头被剥离时，它的续行一并
/// 丢弃 —— 否则续行会变成无主行（既可能被收件端当成前一个保留头的续行，也可能孤零零
/// 触发解析错误）。
fn kept_header_lines(head: &str) -> Vec<&str> {
    let mut kept = Vec::new();
    let mut keep_prev = false;
    for line in head.split_inclusive('\n') {
        let bare = bare_line(line);
        if bare.starts_with(' ') || bare.starts_with('\t') {
            if keep_prev {
                kept.push(line);
            }
            continue;
        }
        // `Name: value` → 取 `Name`；无冒号的行无从判定冲突，按「保留」处理（不猜不吞）。
        let name = bare.split(':').next().unwrap_or(bare);
        keep_prev = !CONFLICTING_HEADERS
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h));
        if keep_prev {
            kept.push(line);
        }
    }
    kept
}

/// 头部区与正文的分界 = 首个空行。返回 `(头部区, 正文)`；无空行 ⇒ 全是头、无正文。
fn split_head_body(raw: &str) -> (&str, &str) {
    let mut end = 0usize;
    for line in raw.split_inclusive('\n') {
        if bare_line(line).is_empty() {
            return (&raw[..end], &raw[end + line.len()..]);
        }
        end += line.len();
    }
    (raw, "")
}

/// 去掉行尾 `\n` 与 `\r`（只看行尾，不动行内内容）。
fn bare_line(line: &str) -> &str {
    let l = line.strip_suffix('\n').unwrap_or(line);
    l.strip_suffix('\r').unwrap_or(l)
}

/// 行尾归一到 CRLF（裸 LF → CRLF；已有 CRLF 不动；裸 CR 保留）。
fn normalize_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut prev_cr = false;
    for ch in s.chars() {
        if ch == '\n' && !prev_cr {
            out.push('\r');
        }
        out.push(ch);
        prev_cr = ch == '\r';
    }
    out
}

/// 拒绝含 CR/LF 的值（头注入纵深防线；主防线是宿主的过线前剥离）。
fn ensure_no_crlf(value: &str, field: &str) -> Result<(), String> {
    if value.contains(['\r', '\n']) {
        return Err(format!(
            "{field} 含 CR/LF（头注入防护）：请去掉换行（下一步：宿主应已剥离，检查该字段来源）"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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

        let html =
            build_message(&req_with(None, Some("<i>h</i>"), vec![]), &[]).expect("html 组装");
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

    // ---- Task 5.2：raw 冲突头剥离 ----

    #[test]
    fn raw_strips_from_to_cc_bcc_subject_case_insensitively() {
        let raw = "From: evil@x\nTo: victim@x\nSUBJECT: spoof-subject\nCc: c@x\nX-Keep: 1\n\nbody";
        let m = build_raw(&envelope(), raw, &[]).expect("剥离");
        let s = String::from_utf8(m).expect("UTF-8");
        assert!(
            s.contains("X-Keep: 1") && s.contains("body"),
            "其余头与正文保留: {s}"
        );
        assert!(
            !s.contains("evil@x") && !s.contains("victim@x"),
            "原文冲突头已剥离: {s}"
        );
        assert!(!s.contains("c@x"), "原文 Cc 已剥离: {s}");
        assert!(
            !s.to_lowercase().contains("subject:"),
            "原文 Subject 行（含大小写变体）必须整行剥离: {s}"
        );
        assert!(
            s.contains("From: from@example.com") && s.contains("To: to@example.com"),
            "报头由结构化信封重建: {s}"
        );
    }

    #[test]
    fn raw_strips_folded_continuation_of_stripped_header_only() {
        let raw = "X-Fold: a\n  keep-me\nFrom: evil@x\n\tleak-me\nX-Two: 2\n\nbody";
        let s = String::from_utf8(build_raw(&envelope(), raw, &[]).expect("剥离")).expect("UTF-8");
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
        let s = String::from_utf8(build_raw(&envelope(), raw, &[]).expect("剥离")).expect("UTF-8");
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
            &[att("a.pdf", "application/pdf", b"x")],
        )
        .expect_err("raw 与附件互斥");
        assert!(e.contains("附件"), "{e}");

        let senderless = Envelope::new(None, vec![addr("to@example.com")]).expect("信封");
        assert!(
            build_raw(&senderless, "X-A: 1\n\nb", &[]).is_err(),
            "缺发件人必须 Err"
        );
    }

    #[test]
    fn raw_without_blank_line_keeps_headers_and_empty_body() {
        let s =
            String::from_utf8(build_raw(&envelope(), "X-A: 1", &[]).expect("剥离")).expect("UTF-8");
        assert!(
            s.contains("X-A: 1\r\n\r\n"),
            "无空行时按「全是头、空正文」处理: {s}"
        );
    }
}
