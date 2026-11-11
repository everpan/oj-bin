// 服务凭据运营端点（spec 2026-11-11 P2）：create / list / revoke。
// 明文 token 只在 create 响应出现一次（不落库不回显）；revoke = 删登记行
// （行即吊销真相源——admit SvcGate 与 obo 换签都以「行存在」为准，fail-closed）。
import { nowSecs } from "../../auth/_shared/session";
import { isAdmin, denyNonAdmin } from "../_shared/guard";

// 档位白名单（与 serve 侧 tier_of 同词表：read/write/destructive）。
const SCOPES = ["read", "write", "destructive"];
const SVC_TTL_SECS = 90 * 86400; // 90d 上限（bridge JWT_TTL_CAP_SECS 同值）

export const schema = {
  description: "服务凭据（svc token）签发 / 列表 / 吊销",
};

async function get(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const rows: any[] = await db.table("oj_svc_token")
    .select(["jti", "name", "scopes", "created_by", "created_at"])
    .orderBy([{ field: "created_at", dir: "desc" }])
    .all();
  json.ok(rows.map((r) => {
    let scopes: string[] = [];
    try { scopes = JSON.parse(String(r.scopes || "[]")); } catch { scopes = []; }
    return { ...r, scopes };
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
  const token = await jwt.sign({
    sub: "svc:" + name,
    roles: ["bot"],
    claims: { svc: name, jti, scopes },
    ttl_secs: SVC_TTL_SECS,
  });
  await db.table("oj_svc_token").insert({
    jti, name, scopes: JSON.stringify(scopes),
    created_by: String(http.user!.id), created_at: nowSecs(),
  }).run();
  json.ok({ token, jti }); // 明文仅此一次
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
