//! 阶段 5：请求反序列化 + 消息组装。
//!
//! 两条互斥的投递路（`engine::submit` 按 `req.raw` 有无二选一）：
//!
//! - **结构化组装**（[`build_message`]）：`from`/`to`/`cc`/`bcc`/`subject`/`text`/`html`/
//!   `headers`/`attachments` → `lettre::Message`（text+html → `multipart/alternative`；
//!   有附件 → 外层 `multipart/mixed`）。信封（MAIL FROM / RCPT TO）由 lettre 从报头派生
//!   （To ∪ Cc ∪ Bcc；`Bcc` 报头在派生后按 lettre 默认丢弃 → 收件人可见性正确）。
//!   正文（`text`/`html`）行尾一律归一 CRLF（B4：裸 CR 是 SMTP smuggling 半开面）。
//! - **原文投递**（[`build_raw`]）：`raw` 是调用方自备的 RFC5322 原文，由本模块拼出最终字节。
//!   **只剥离信封头/身份头 `From`/`To`/`Cc`/`Bcc`/`Sender`/`Return-Path`**（信封的权威
//!   来源是结构化 `from`/`to`，原文里这些头留着就能造出双收件人 / 发件人 spoof，含折行
//!   续行一并丢弃）；`Subject` **保留**（非信封字段，剥它只会丢主题），但做 CRLF 校验，
//!   且**结构化 `subject` 非空时覆盖原文 Subject**。
//!   其余头与正文逐字节保留（正文行尾归一 CRLF，裸 CR 也归一；头区的裸 CR 一律拒绝，
//!   见 [`build_raw`] 的说明）。**不**经 lettre 的 MIME 组装 —— `Message::body` 会按
//!   「最优编码」重编码正文（行 ≥76 字节即改用 quoted-printable/base64），已编码的
//!   multipart 原文会被改烂。
//!
//! 决策依据：设计 §7/§11（2026-09-15 controller 决策 —— `Subject` 非信封字段，不剥离）。
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
use lettre::message::header::{ContentType, HeaderName, HeaderValue, Headers, Subject};
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use oj_plugin_ffi::MailAttachment;
use serde::Deserialize;
use std::collections::HashMap;

/// raw 路必须剥离的**信封头 / 身份头**（**大小写不敏感**）：信封（MAIL FROM / RCPT TO）的
/// 权威来源是结构化 `from`/`to`，原文里这些头留着就能造出「双收件人 / 发件人 spoof」。
///
/// `Sender`/`Return-Path`（B5）同列：二者都断言「谁把这封信交给 MTA」——
/// 原文留着就会与重建的 `From`（来自结构化信封）矛盾，构成弱 spoof 面
/// （`Return-Path` 按 RFC 5321 本就只由收信方在投递时添加，客户端不得发）。
///
/// `Subject` **不在**此列：它不是信封字段，剥掉只会让邮件丢主题；原文 Subject 的注入面由
/// CRLF 校验覆盖（见 [`build_raw`]）。`Reply-To` 也不在：raw 是调用方自备的原文，
/// 它不改变信封与发件人身份（结构化 `headers` 路则禁用，见 [`STRUCTURED_HEADERS`]）。
const ENVELOPE_HEADERS: [&str; 6] = ["from", "to", "cc", "bcc", "sender", "return-path"];

/// 结构化字段权威、**不允许** `headers` 覆盖的头（含 `Subject`：主题由 `subject` 决定）。
/// 组装路的信封是 lettre **由报头派生**的，允许覆盖 = 绕过宿主收件人白名单的面。
///
/// B5 补入三个**弱 spoof 面**头：`Sender`（实际提交者，与 `From` 不一致即冒充）、
/// `Return-Path`（退回地址，客户端本就不该发）、`Reply-To`（把回复引到别处 —— 钓鱼面）。
const STRUCTURED_HEADERS: [&str; 8] = [
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "sender",
    "return-path",
    "reply-to",
];

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
    /// 主题。raw 路见模块头 / [`build_raw`]：原文 `Subject:` **保留**（非信封字段），
    /// 但本字段**非空时覆盖**它（最终只有一个 Subject 头）。
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
        // 正文行尾一律归一 CRLF（B4）：裸 CR 会被中间 MTA 当行界 → SMTP smuggling 半开
        // （正文里的 `X\r.\r\n` 会让对方提前结束 DATA，余下被当命令）。
        (Some(t), Some(h)) => Some(Content::Multi(MultiPart::alternative_plain_html(
            normalize_crlf(t),
            normalize_crlf(h),
        ))),
        (Some(t), None) => Some(Content::Single(SinglePart::plain(normalize_crlf(t)))),
        (None, Some(h)) => Some(Content::Single(SinglePart::html(normalize_crlf(h)))),
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
    if STRUCTURED_HEADERS
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

/// raw 路：剥离信封头后的**最终 RFC5322 字节**（信封由调用方另行交给 transport）。
///
/// 与 [`build_message`] 不同，这里不经 lettre 组装：原文（除信封头与行尾）逐字节保留。
/// 失败一律 `Err(原因 + 下一步)`（由 `engine` 映射为 `code:5`）。
///
/// ## 头语义（设计 §7/§11，2026-09-15 决策）
///
/// - `From`/`To`/`Cc`/`Bcc`：**一律剥离**（含折行续行），报头改由结构化信封重建 —— 防双收件人 /
///   发件人 spoof；SMTP 信封（MAIL FROM / RCPT TO）本就只认结构化 `from`/`to`。
/// - `Subject`：**保留**（非信封字段）。`subject` 非空 ⇒ 以它为准**覆盖**（原文 Subject 行连同
///   其折行续行整行丢弃，最终只有一个 Subject 头）；`subject` 为空 ⇒ 原文 Subject 原样保留。
/// - 其余头与正文逐字节保留，但原文 Subject 值要过 CRLF 校验（裸 CR = 头注入面 ⇒ `Err`）。
///
/// ## 为什么不让 lettre 组装 raw
///
/// `MessageBuilder::body` 会按「最优编码」重编码正文（行 ≥76 字节就改 quoted-printable /
/// base64），而 `Content-Type`/`Content-Transfer-Encoding` 等原文报头是**原样保留**的 ——
/// 一个 base64 分块的 multipart 原文会被 `multipart/*` 报头 + base64 正文的组合改烂
/// （收件端解析不出任何 part）。故 raw 路自己拼字节。
///
/// ## 行尾一律归一 CRLF（正文）/ 头区裸 CR 一律拒绝
///
/// SMTP DATA 的帧界是 CRLF，而 lettre 的送出侧只做**点填充**（`ClientCodec`）不做行尾
/// 归一：裸 LF/裸 CR 之后的行首 `.` 不会被填充，中间 MTA 可据此提前结束 DATA → 余下内容被当
/// 命令执行（SMTP smuggling）。故正文（含 raw 正文）的行尾统一成 CRLF（[`normalize_crlf`]，
/// 裸 CR 也在内）；**头区**若含裸 CR 直接 `Err`（归一它会凭空造出一个头，见
/// [`kept_header_lines`]）。
pub fn build_raw(
    envelope: &Envelope,
    raw: &str,
    subject: &str,
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
    if !subject.is_empty() {
        ensure_no_crlf(subject, "subject")?;
    }
    let (head, body) = split_head_body(raw);

    let mut out = String::with_capacity(raw.len() + 96);
    // 报头自结构化信封重建（原文的 From/To/Cc/Bcc 已剥离 → 不存在双收件人/spoof）。
    out.push_str(&format!("From: {from}\r\n"));
    let rcpt: Vec<String> = envelope.to().iter().map(Address::to_string).collect();
    out.push_str(&format!("To: {}\r\n", rcpt.join(", ")));
    if !subject.is_empty() {
        out.push_str(&subject_header(subject));
    }
    for line in kept_header_lines(head, subject.is_empty())? {
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n'); // 无空行的「全是头」原文：补上行尾，别把正文粘到最后一个头上
        }
    }
    out.push_str("\r\n"); // 头/正文分隔空行
    out.push_str(body);
    Ok(normalize_crlf(&out).into_bytes())
}

/// 用 lettre 的头编码器产出 `Subject: <encoded>\r\n`（RFC2047 编码 + 折行都由 lettre 负责）。
///
/// 复用 `Headers` 而非手写编码：与组装路同一套实现，避免出现第二份 RFC2047 逻辑。
fn subject_header(value: &str) -> String {
    let mut headers = Headers::new();
    headers.set(Subject::from(value.to_string()));
    headers.to_string()
}

/// 头部区中**保留**的行（含原行尾）：剥离信封头/身份头，其余头（及其折行续行）原样保留。
///
/// 折行（continuation，行首为空格/TAB）归属**上一个头**：上一个头被丢弃时，它的续行一并
/// 丢弃 —— 否则续行会变成无主行（既可能被收件端当成前一个保留头的续行，也可能孤零零
/// 触发解析错误）。
///
/// `keep_subject` = 结构化 `subject` 为空：为真时原文 Subject（含折行续行）保留并要求通过
/// CRLF 校验；为假时原文 Subject 整段丢弃（让结构化值成为唯一的 Subject）。
///
/// **保留的每一行都不得含裸 CR**（B4）：头区里的裸 CR 在「以裸 CR 为行界」的接收端就是
/// 换行 —— 归一它会**凭空造出一个头**（`X-Foo: a\rBcc: x` → 两条头），比什么都糟；
/// 故此处**拒绝**（`code:5`），与正文的归一处置互补（正文归一不产生新头）。
fn kept_header_lines(head: &str, keep_subject: bool) -> Result<Vec<&str>, String> {
    let mut kept = Vec::new();
    let mut keep_prev = false;
    let mut prev_is_subject = false;
    for line in head.split_inclusive('\n') {
        let bare = bare_line(line);
        if bare.starts_with(' ') || bare.starts_with('\t') {
            if keep_prev {
                // 续行同样可能夹带裸 CR（行扫描只认 `\n`）—— 保留它就等于保留注入面。
                ensure_no_crlf(bare, "raw 头（折行续行）")?;
                kept.push(line);
            }
            continue;
        }
        // `Name: value` → 取 `Name`；无冒号的行无从判定，按「保留」处理（不猜不吞）。
        let name = bare.split(':').next().unwrap_or(bare);
        prev_is_subject = name.eq_ignore_ascii_case("subject");
        keep_prev = if prev_is_subject {
            keep_subject
        } else {
            !ENVELOPE_HEADERS
                .iter()
                .any(|h| name.eq_ignore_ascii_case(h))
        };
        if keep_prev {
            // 保留头的裸 CR 一律拒（归一 = 造头）。
            ensure_no_crlf(bare, "raw 头")?;
            kept.push(line);
        }
    }
    Ok(kept)
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

/// 行尾一律归一 CRLF（B4）：`CR` / `LF` / `CRLF` 三种行尾**都**映射成 CRLF。
///
/// 裸 CR（非 CRLF 的 `\r`）此前被原样保留，于是正文里的 `X\r.\r\n` 在「以裸 CR 为行界」
/// 的接收端/中间 MTA 上会提前结束 DATA —— 余下内容被当命令执行（SMTP smuggling，
/// 可注入伪造的 MAIL FROM / RCPT TO）。归一后 `X\r.\r\n` → `X\r\n.\r\n`，那条 `.` 成为
/// **正常行首的点**，由 lettre 送出侧的点填充（dot-stuffing）正确转义。
///
/// **口径选择（归一而非拒绝）**：与「裸 LF → CRLF」已有的处置一致 —— 同一类「行尾写错」
/// 不该有两种相反处置（LF 归一、CR 拒绝会让人难以预期）；且裸 CR 在真实正文里存在
/// （Windows 剪贴板 / 老式 Mac 行尾），拒绝会无谓地打断发信。**头区**是例外：那里裸 CR
/// 一律**拒绝**（见 [`kept_header_lines`]）—— 归一等于凭空造出一个新头（头注入）。
fn normalize_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        match ch {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next(); // CRLF：整体一个行尾
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            _ => out.push(ch),
        }
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
        let s =
            String::from_utf8(build_raw(&envelope(), raw, "", &[]).expect("剥离")).expect("UTF-8");
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
        let s =
            String::from_utf8(build_raw(&envelope(), raw, "", &[]).expect("剥离")).expect("UTF-8");
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
        let s = String::from_utf8(build_raw(&envelope(), "X-A: 1", "", &[]).expect("剥离"))
            .expect("UTF-8");
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
        let s = String::from_utf8(
            build_raw(&envelope(), "X-A: 1\n\nX\r.\r\ntail", "", &[]).expect("组装"),
        )
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
        let s =
            String::from_utf8(build_raw(&envelope(), "From: a\rX\n\nbody", "", &[]).expect("剥离"))
                .expect("UTF-8");
        assert!(!s.contains("From: a\rX"), "被剥离头不得残留: {s:?}");
    }

    /// B4：结构化 `text`/`html` 正文里的裸 CR 同样归一（组装路是另一条写正文的路径）。
    #[test]
    fn structured_bodies_normalize_bare_cr() {
        let req = req_with(Some("X\r.\r\ntail"), Some("<b>a\rb</b>"), vec![]);
        let s =
            String::from_utf8(build_message(&req, &[]).expect("组装").formatted()).expect("UTF-8");
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
        let s =
            String::from_utf8(build_raw(&envelope(), raw, "", &[]).expect("剥离")).expect("UTF-8");
        let low = s.to_lowercase();
        assert!(!low.contains("sender:") && !low.contains("leak"), "{s}");
        assert!(!low.contains("return-path") && !s.contains("evil@x"), "{s}");
        assert!(s.contains("Reply-To: r@x"), "Reply-To 保留: {s}");
        assert!(s.contains("X-Keep: 1") && s.contains("body"), "{s}");
    }
}
