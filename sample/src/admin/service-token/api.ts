// 服务凭据运营端点（spec 2026-11-11 P2）：create / list / revoke。
// R6（v0.1.132）起明文 token 落库（token 列）——跨重启恢复面 GET current（svc 自
// 认证）依赖它；admin list 永不选择该列，明文暴露面 = create 响应 + current（同主体）。
// revoke = 删登记行（行即吊销真相源——admit SvcGate 与 obo 换签都以「行存在」为准，
// fail-closed）；R6 起轮换走 superseded + grace_until（旧 jti 宽限 24h 后自动移出）。
import { nowSecs } from "../../auth/_shared/session";
import { isAdmin, denyNonAdmin } from "../_shared/guard";
// SVC_TTL_SECS 唯一源在 system/service-token/_shared（rotate/current 与签发共用，
// 评审修复：原两处各自硬编码 90d，靠注释互指，改动必漂移）。
import { SVC_TTL_SECS, GRACE_SECS } from "../../system/service-token/_shared";

// 档位白名单（与 serve 侧 tier_of 同词表：read/write/destructive）。
const SCOPES = ["read", "write", "destructive"];

export const schema = {
  description: "服务凭据（svc token）签发 / 列表 / 吊销",
};

async function get(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const rows: any[] = await db.table("oj_svc_token")
    .select(["jti", "name", "scopes", "created_by", "created_at",
      "status", "grace_until", "expires_at"]) // 永不选择 token 列（明文面不扩）
    .orderBy([{ field: "created_at", dir: "desc" }])
    .all();
  json.ok(rows.map((r) => {
    let scopes: string[] = [];
    try { scopes = JSON.parse(String(r.scopes || "[]")); } catch { scopes = []; }
    return {
      ...r,
      scopes,
      // bigint 列跨驱动形态不一（pg 可能文本到达，svc_gate.rs 有同款宽容解析，
      // 评审 S4）：admin 面统一成 number，运营脚本按 dialect 不漂移。NULL 保持 NULL。
      grace_until: r.grace_until == null ? null : Number(r.grace_until),
      expires_at: r.expires_at == null ? null : Number(r.expires_at),
    };
  }));
}

async function post(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const b = (http.body || {}) as { name?: unknown; scopes?: unknown };
  const name = String(b.name ?? "").trim();
  if (!name) { json.fail(400, "name required"); return; }
  let scopes: string[] = Array.isArray(b.scopes) ? b.scopes.map(String) : ["read"];
  const bad = scopes.filter((s) => !SCOPES.includes(s));
  if (bad.length) { json.fail(400, "scopes must be a subset of: " + SCOPES.join("/")); return; }
  if (!scopes.length) { json.fail(400, "scopes must not be empty"); return; }
  const jti = crypto.randomHex(16);
  const expiresAt = nowSecs() + SVC_TTL_SECS;
  const token = await jwt.sign({
    sub: "svc:" + name,
    roles: ["bot"],
    claims: { svc: name, jti, scopes },
    ttl_secs: SVC_TTL_SECS,
  });
  // 驱旧 + 落新同事务（评审 #6）：顺序 admin 双 POST 不再产生两行 active。
  // 先插新 active 行、再条件驱旧（除本行外的 active/存量 NULL 行）——并发双 POST
  // 也收敛到恰好一行 active（后到者的驱旧会把先到者的新行翻成 superseded，与
  // rotate 条件栅栏同款的「最后写入者胜」，响应各自 200 但旧行有 24h 宽限可感知）。
  await db.tx(async (tx: any) => {
    await tx.table("oj_svc_token").insert({
      jti, name, scopes: JSON.stringify(scopes),
      created_by: String(http.user!.id), created_at: nowSecs(),
      status: "active", token, expires_at: expiresAt, // R6：落库供 current 恢复
    }).run();
    await tx.table("oj_svc_token").update({
      status: "superseded",
      grace_until: nowSecs() + GRACE_SECS,
    }).where({ and: [
      { field: "name", op: "eq", value: name },
      { field: "jti", op: "ne", value: jti },
      { or: [
        { field: "status", op: "isnull" },
        { field: "status", op: "eq", value: "active" },
      ] },
    ] }).run();
  });
  json.ok({ token, jti, expires_at: expiresAt });
}

async function del(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const jti = String(http.param("jti", ""));
  const n = await db.table("oj_svc_token").delete()
    .where({ field: "jti", op: "eq", value: jti }).run();
  if (!n) { json.fail(404, "no such token"); return; }
  json.ok({ revoked: n });
}

del.route = "{jti}";
export default { get, post, del };
