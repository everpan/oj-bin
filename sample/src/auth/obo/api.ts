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
    // 40311 已吊销：**不在本 handler 重查登记表**（评审 #10）——dispatch 的 ②
    // jti 门（svc 标记 token 全动词覆盖，/auth/obo 无豁免）已按 SvcGate 的
    // status/grace 真相源 fail-closed，先于 handler 执行；此处再查「行存在」是
    // 第二真相源（行永不删，过期 superseded 行也会被当成有效），且一旦未来出现
    // 绕过 admit 的通路（in-process 调用等）即失效。jti claim 缺失同判（验签层
    // 不可能发生，纵深防御保留）。
    if (typeof jti !== "string" || !jti) {
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
