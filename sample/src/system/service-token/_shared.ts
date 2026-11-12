// R6 svc 凭据自轮换共享件（v0.1.132）：由「当前有效 svc bearer 自身」认证——
// 守卫已验签 + SvcGate 已查 jti（fail-closed，宽限内旧 jti 也算有效），这里只取
// 身份（claims.svc / claims.jti）。密钥不出 oj：新 token 仍由 jwt.sign 用同一
// secret 签发（消费方零 admin 介入、零密钥暴露）。
import { nowSecs } from "../../auth/_shared/session";

export const SVC_TTL_SECS = 90 * 86400; // 90d 上限（与 admin 签发端点同值）
export const GRACE_SECS = 24 * 3600; // 旧 jti 宽限窗（在途请求平滑过渡）

export interface SvcSelf {
  name: string; // claims.svc
  jti: string; // claims.jti
}

// 非 svc bearer（员工 token / 匿名）→ null，调用方 403。
export function requireSvc(): SvcSelf | null {
  const c: any = (http.user as any)?.claims;
  if (!c || typeof c.svc !== "string" || typeof c.jti !== "string") return null;
  return { name: c.svc, jti: c.jti };
}

export { nowSecs };
