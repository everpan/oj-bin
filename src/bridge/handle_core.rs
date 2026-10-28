//! 运行期「凭据句柄」共性件（U-4 v0.1.95）。
//!
//! 跨请求的宿主级句柄（事务会话 `db.session`，以及后续 U-5 的执行登记）共用三件事：
//! **不可枚举的 id**、**归属主体**、**空闲/寿命双计时**。抽在此处是为了让 U-5 复用同一套
//! 判定与日志遮蔽规则，而不是提前造泛型表（两边的载荷差异大：事务要持
//! `Box<dyn TxSession>` 与方言快照，取消只有一条取消通道，泛型化收益不抵复杂度）。
//!
//! # 安全约定（红线）
//!
//! - id 一律 `getrandom` 生成；**失败返回 `Err`，绝不退回时间戳/自增**（可枚举 = 跨租户打靶面）。
//! - 任何日志/错误消息只允许出现 [`id_prefix`]（前 8 字符 + `…`）：完整 id 等价 bearer token。
//! - 外来 id 先做**形状白名单**校验再用；绝不按字节切片（多字节 UTF-8 会 panic，
//!   换行还会伪造日志行）。

use std::time::Duration;

use tokio::time::Instant;

/// 句柄归属主体。`tenant`/`user` 参与判定；`module`/`source` 仅审计
/// （跨请求流程常跨模块，按 module 收窄会让正常用法随机失败）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub tenant: Option<String>,
    pub user: Option<String>,
    pub module: Option<String>,
    pub source: &'static str,
}

impl Owner {
    /// 是否同一主体（= 可操作对方句柄）。`module`/`source` 故意不参与。
    pub fn same_subject(&self, other: &Self) -> bool {
        self.tenant == other.tenant && self.user == other.user
    }

    /// 是否携带身份（`require_identity` 的判定依据）。
    pub fn has_identity(&self) -> bool {
        self.user.is_some() || self.tenant.is_some()
    }
}

/// 每进程启动标签（4 hex）：让日志/`info()` 能区分「上个进程遗留的 id」。
///
/// 不是安全边界（真随机在 id 本身）；`getrandom` 不可用时退化为时间派生，
/// 不让启动失败。
pub fn new_boot_tag() -> String {
    let mut b = [0u8; 2];
    if getrandom::getrandom(&mut b).is_err() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        return format!("{:04x}", (n & 0xffff) as u16);
    }
    hex(&b)
}

/// 生成句柄 id：`<prefix><boot4>_<32hex>`。`getrandom` 失败 → `Err`（不 panic、不退化）。
pub fn new_id(prefix: &str, boot: &str) -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| format!("os random unavailable: {e}"))?;
    Ok(format!("{prefix}{boot}_{}", hex(&b)))
}

/// id 形状白名单：`<prefix><4 hex>_<32 hex>`。外来输入必须先过这一关。
pub fn id_is_wellformed(prefix: &str, id: &str) -> bool {
    let expect = prefix.len() + 4 + 1 + 32;
    if id.len() != expect || !id.starts_with(prefix) {
        return false;
    }
    let rest = &id[prefix.len()..];
    let (boot, tail) = match rest.split_once('_') {
        Some(p) => p,
        None => return false,
    };
    boot.len() == 4 && tail.len() == 32 && is_hex(boot) && is_hex(tail)
}

/// 日志/错误里能出现的 id 形态：`tx_ab12_…`（前 8 字符 + 省略号）。
/// 形状非法的输入不给回声（避免把攻击者的字节写进日志）。
pub fn id_prefix(prefix: &str, id: &str) -> String {
    if !id_is_wellformed(prefix, id) {
        return format!("{prefix}<malformed>");
    }
    let mut s: String = id.chars().take(8).collect();
    s.push('…');
    s
}

/// 双计时余量：`min(空闲余量, 寿命余量)`，到点即 0（饱和，不 panic）。
pub fn expires_in(
    created: Instant,
    last_used: Instant,
    idle_timeout: Duration,
    max_lifetime: Duration,
    now: Instant,
) -> Duration {
    let idle_left = idle_timeout.saturating_sub(now.saturating_duration_since(last_used));
    let life_left = max_lifetime.saturating_sub(now.saturating_duration_since(created));
    idle_left.min(life_left)
}

/// 过期原因（`None` = 未过期）。寿命优先于空闲（两者同时到点时报告更强的那个）。
pub fn expired_reason(
    created: Instant,
    last_used: Instant,
    idle_timeout: Duration,
    max_lifetime: Duration,
    now: Instant,
) -> Option<&'static str> {
    if now.saturating_duration_since(created) >= max_lifetime {
        return Some("max_lifetime");
    }
    if now.saturating_duration_since(last_used) >= idle_timeout {
        return Some("idle_timeout");
    }
    None
}

fn is_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_generated_id_when_checked_then_wellformed_and_prefix_masked() {
        let boot = new_boot_tag();
        assert_eq!(boot.len(), 4);
        let id = new_id("tx_", &boot).unwrap();
        assert!(id_is_wellformed("tx_", &id), "{id}");
        assert_eq!(id.len(), 40);
        let p = id_prefix("tx_", &id);
        assert_eq!(p.chars().count(), 9, "{p}");
        assert!(p.ends_with('…'));
        // 遮蔽后不得含完整 id（等于 token 泄漏）
        assert!(!p.contains(&id));
    }

    #[test]
    fn given_two_ids_then_distinct() {
        let boot = new_boot_tag();
        let a = new_id("tx_", &boot).unwrap();
        let b = new_id("tx_", &boot).unwrap();
        assert_ne!(a, b);
    }

    /// 对抗：多字节 / 换行 / 超长 / 大写十六进制 / 错前缀 一律非法，
    /// 且 `id_prefix` 不回显原文（防日志注入）。
    #[test]
    fn given_hostile_id_when_validated_then_rejected_without_echo() {
        for bad in [
            "tx_ab12_…",
            "tx_ab12_",
            "tx_ABCD_00000000000000000000000000000000",
            "tx_ab12_00000000000000000000000000000000\n",
            "tx_ab12_0000000000000000000000000000000g",
            "粗_ab12_00000000000000000000000000000000",
        ] {
            assert!(!id_is_wellformed("tx_", bad), "{bad:?}");
            let p = id_prefix("tx_", bad);
            assert_eq!(p, "tx_<malformed>");
            assert!(!p.contains('\n'));
        }
    }

    #[test]
    fn given_idle_and_lifetime_when_computing_then_min_wins() {
        let now = Instant::now();
        let created = now - Duration::from_secs(10);
        let last = now - Duration::from_secs(2);
        // 空闲余量 8s、寿命余量 20s → 8s
        assert_eq!(
            expires_in(
                created,
                last,
                Duration::from_secs(10),
                Duration::from_secs(30),
                now
            ),
            Duration::from_secs(8)
        );
        // 寿命余量 3s 更紧 → 3s
        assert_eq!(
            expires_in(
                created,
                last,
                Duration::from_secs(10),
                Duration::from_secs(13),
                now
            ),
            Duration::from_secs(3)
        );
        // 已过期 → 0（饱和，不 panic）
        assert_eq!(
            expires_in(
                created,
                last,
                Duration::from_secs(1),
                Duration::from_secs(5),
                now
            ),
            Duration::ZERO
        );
        assert_eq!(
            expired_reason(
                created,
                last,
                Duration::from_secs(10),
                Duration::from_secs(5),
                now
            ),
            Some("max_lifetime")
        );
        assert_eq!(
            expired_reason(
                created,
                last,
                Duration::from_secs(1),
                Duration::from_secs(30),
                now
            ),
            Some("idle_timeout")
        );
        assert_eq!(
            expired_reason(
                created,
                last,
                Duration::from_secs(30),
                Duration::from_secs(60),
                now
            ),
            None
        );
    }

    #[test]
    fn given_owners_when_comparing_then_subject_is_tenant_plus_user() {
        let a = Owner {
            tenant: Some("t".into()),
            user: Some("u".into()),
            module: Some("m1".into()),
            source: "http",
        };
        let mut b = a.clone();
        b.module = Some("m2".into());
        assert!(a.same_subject(&b), "模块名不参与判定");
        b.user = Some("u2".into());
        assert!(!a.same_subject(&b));
        assert!(a.has_identity());
        assert!(
            !Owner {
                tenant: None,
                user: None,
                module: Some("m".into()),
                source: "http"
            }
            .has_identity()
        );
    }
}
