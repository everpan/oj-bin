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
pub struct MailVtable {
    /// 投递一封信。
    ///
    /// - `key` = `smtp` 配置里的 profile 名；**未知 key → Err**（不回落 default，
    ///   错配的 profile 名必须显式失败，避免静默发错邮件）。
    /// - `req` = 请求 JSON，字段语义：
    ///   - `sync: bool`：用同步 transport（worker 内 spawn_blocking）还是异步 transport；
    ///   - `enqueue_only: bool`：`true` 时本 future 只立即回 `{"jobId":"..."}`（入队即返回，
    ///     真实投递结果经 `HostContext.deliver("mail.result", ...)` 上送）；`false` 时本
    ///     future 的 resolve 值就是投递结果信封；
    ///   - `raw: Option<String>`：给定时按**原始 MIME** 投递（此时 `subject`/`text`/`html`
    ///     等组装字段被忽略，与 `req` 其余字段冲突的头由宿主剥离）；
    ///   - 其余组装字段（`from`/`to[]`/`cc[]`/`bcc[]`/`subject`/`text?`/`html?`/`headers{}`/`jobId`）。
    /// - `atts` = 附件表（**与 `raw` 互斥**：`raw` 给定时宿主不解析附件，`atts` 为空）。
    ///   字节是宿主解析好的**原始字节**，插件直接喂 lettre，不做 base64 往返。
    pub submit: extern "C" fn(key: RString, req: RString, atts: RVec<MailAttachment>) -> FfiFuture,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_attachment_holds_raw_bytes() {
        let a = MailAttachment {
            filename: RString::from("a.pdf"),
            mime: RString::from("application/pdf"),
            bytes: RBytes::from(&[0u8, 159, 255][..]),
        };
        // 原始字节逐位相等（非 base64、非 UTF-8 假设）。
        assert_eq!(a.bytes.as_slice(), &[0u8, 159, 255][..]);
        assert_eq!(&a.filename[..], "a.pdf");
    }
}
