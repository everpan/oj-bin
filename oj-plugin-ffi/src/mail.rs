//! mail 轴 vtable（新增轴，ABI 不变——spec「加轴零破坏」）。
//! 契约形态对齐既有轴：仅 `submit` 一个 extern "C" 入口，方法面变化由 req JSON
//! 的字段承载（加字段零 ABI 变更，同 mq 的 JSON method dispatch 思路）。
//! 附件字节在**宿主**解析（blob / 本地文件）后以 `RBytes` 原样过线，插件直接喂
//! lettre，不经 JSON/base64——避免大附件在两侧各做一轮编解码。

use crate::{FfiFuture, RBytes, RString, RVec};

/// 附件：宿主解析后的**原始字节**（非 base64），插件直接喂 lettre。
#[stabby::stabby]
#[repr(C)]
pub struct MailAttachment {
    pub filename: RString,
    pub mime: RString,
    pub bytes: RBytes,
}

/// mail 轴：`submit` 统一入口，行为由 req JSON 的 `sync`/`enqueue_only`/`raw` 决定。
/// ok 值 = 结果信封 JSON；`enqueue_only` 时 future 立即回 `{"jobId":"..."}`，
/// 真实完成经 `HostContext.deliver("mail.result", ...)` 上送。
#[stabby::stabby]
#[repr(C)]
pub struct MailAxis {
    pub submit: extern "C" fn(key: RString, req: RString, atts: RVec<MailAttachment>) -> FfiFuture,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_attachment_roundtrips_bytes_without_base64() {
        let a = MailAttachment {
            filename: RString::from("a.pdf"),
            mime: RString::from("application/pdf"),
            bytes: RBytes::from(&[0u8, 159, 255][..]),
        };
        assert_eq!(a.bytes.len(), 3); // 原始字节，非 base64
        assert_eq!(String::from(a.filename.clone()), "a.pdf");
    }
}
