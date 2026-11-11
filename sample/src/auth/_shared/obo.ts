// OBO 换签共享助手（spec 2026-11-11 P2）：auth/obo 端点与未来 bot 骨架复用。
// 业务码冻结（Task 10 e2e 按信封 code 断言；>599 的 code 经 clamp_status
// HTTP 侧落 500——消费方一律读信封 code，不读 HTTP 状态）。
import { nowSecs } from "./session";

export const OBO = {
  UNBOUND: 40310, // provider+external_sub 无绑定行
  REVOKED: 40311, // jti 不在 oj_svc_token（已吊销/不存在；fail-closed）
  NOT_SVC: 40312, // claims.svc 缺失/非串（普通员工 token）
  DISABLED: 40313, // users.disabled==1
} as const;

export const OBO_MSG: Record<number, string> = {
  [OBO.UNBOUND]: "该账号未绑定机器人代操作，请联系管理员",
  [OBO.REVOKED]: "服务凭据已吊销或不存在",
  [OBO.NOT_SVC]: "仅服务凭据（svc token）可换签员工身份",
  [OBO.DISABLED]: "该账号已停用",
};

export interface OboActor {
  svc: string;
  provider: string;
  external_sub: string;
  user_id: string;
}

// 每次换签调用（无论成败）写一行审计（plan Task 6 Produces）。
export async function auditObo(a: OboActor, ok: boolean, reason: string): Promise<void> {
  await db
    .table("oj_obo_audit")
    .insert({
      svc: a.svc,
      provider: a.provider,
      external_sub: a.external_sub,
      user_id: a.user_id,
      ok: ok ? 1 : 0,
      reason,
      ts: nowSecs(),
    })
    .run();
}
