// cert 模块共享：admin 门禁、参数校验、证书状态推导（无 api.ts → 非路由目录）。

// admin 门禁：http.user 由 auth 中间件验签注入；缺失或非 admin 视为拒绝。
export function isAdmin(): boolean {
  return !!http.user && (http.user.roles ?? []).includes("admin");
}

// 有效天数：整数 1..=3650，非法返回 null。
export function parseDays(raw: unknown): number | null {
  const d = Number(raw ?? 365);
  return Number.isInteger(d) && d >= 1 && d <= 3650 ? d : null;
}

// 证书状态：与 server/src/certificate.rs 判定一致（grace 缺省 30 天）。
export function statusOf(nbf: number, exp: number, now: number): string {
  if (now < exp) return "valid";
  if (now < exp + 30 * 86400) return "grace";
  return "expired";
}
