// OBO 换签端点（spec 2026-11-11 P2）：机器人以 svc token 代表员工换 10-min
// 受托 token。守卫已验签（Bearer svc token，roles:["bot"]）；本端点做：
// 非 svc 拒 → jti 吊销拒 → 绑定缺失拒 → 员工停用拒 → 实时查 roles 换签。
// 每次调用（成败均）落一行 oj_obo_audit。
import { nowSecs } from "../_shared/session";
import { OBO, OBO_MSG, auditObo } from "../_shared/obo";

const OBO_TTL_SECS = 600;

export default {
  async post() {
    const b = (http.body || {}) as { provider?: unknown; external_sub?: unknown };
    const provider = String(b.provider ?? "");
    const externalSub = String(b.external_sub ?? "");
    const claims = ((http.user && http.user.claims) || {}) as Record<string, unknown>;
    const svcName = claims.svc;
    const jti = claims.jti;
    const audit = (uid: string, ok: boolean, reason: string) =>
      auditObo(
        {
          svc: typeof svcName === "string" ? svcName : "",
          provider,
          external_sub: externalSub,
          user_id: uid,
        },
        ok,
        reason,
      );

    // 40312 非 svc 凭据：claims.svc 缺失/非串，或 roles 不含 bot（spec P2 第一步的
    // 纵深防御——svc 名与 bot 角色双查；admin 签发恒 roles:["bot"]，手签漏 bot
    // 角色的 token 在此拦下）。
    const uroles = ((http.user && http.user.roles) || []) as unknown[];
    if (typeof svcName !== "string" || !svcName || !uroles.includes("bot")) {
      await audit("", false, "not_svc");
      json.fail(OBO.NOT_SVC, OBO_MSG[OBO.NOT_SVC]);
      return;
    }
    // 40311 已吊销：登记表查无此 jti（fail-closed；缺 jti claim 同判）。revoke =
    // 物理删行（行即吊销真相源），而 dispatch ② 门的 SvcGate 正缓存有 30s 吊销
    // 滞后（/mcp 在途平滑，设计内）——OBO 是换签放大器，必须此处同步点查收口
    // （spec Task 6 契约：revoke 后立即 40311）。superseded 宽限行仍属「行存在」，
    // 由 ② 门按 status/grace 真相源同步拦（先于本 handler），两查不矛盾。
    if (typeof jti !== "string" || !jti) {
      await audit("", false, "revoked");
      json.fail(OBO.REVOKED, OBO_MSG[OBO.REVOKED]);
      return;
    }
    const tok = await db
      .table("oj_svc_token")
      .select(["jti"])
      .where({ field: "jti", op: "eq", value: jti })
      .all();
    if (!tok.length) {
      await audit("", false, "revoked");
      json.fail(OBO.REVOKED, OBO_MSG[OBO.REVOKED]);
      return;
    }
    // 40310 未绑定：(provider, external_sub) 无绑定行。
    const bindCond = {
      and: [
        { field: "provider", op: "eq", value: provider },
        { field: "external_sub", op: "eq", value: externalSub },
      ],
    };
    const bind = await db.table("oj_bot_binding").select(["user_id"]).where(bindCond).all();
    if (!bind.length) {
      await audit("", false, "unbound");
      json.fail(OBO.UNBOUND, OBO_MSG[OBO.UNBOUND]);
      return;
    }
    const uid = String(bind[0].user_id);
    // 绑定活跃度（运营观测；失败路径不更新）。
    await db.table("oj_bot_binding").update({ last_used_at: nowSecs() }).where(bindCond).run();
    // 40313 停用 + roles 实时查（同一行一次取齐）。
    const rows = await db
      .table("users")
      .select(["id", "roles", "disabled"])
      .where({ field: "id", op: "eq", value: uid })
      .all();
    const row = rows[0];
    if (!row || Number(row.disabled) === 1) {
      await audit(uid, false, "disabled");
      json.fail(OBO.DISABLED, OBO_MSG[OBO.DISABLED]);
      return;
    }
    let roles: string[] = [];
    try {
      roles = JSON.parse(String(row.roles || "[]"));
    } catch {
      roles = [];
    }
    // 受托标记 act（admit act_mask 与审计都按它识别代操作会话）；
    // tenant 原样复制（缺失则不带；PR-9 租户只认验签后的 claim）。
    const extra: Record<string, unknown> = { act: { sub: "svc:" + svcName } };
    if (typeof claims.tenant === "string") extra.tenant = claims.tenant;
    const access_token = await jwt.sign({
      sub: String(row.id),
      roles,
      claims: extra,
      ttl_secs: OBO_TTL_SECS,
    });
    await audit(uid, true, "");
    json.ok({ access_token, expires_in: OBO_TTL_SECS });
  },
};
