// POST /v1/api/system/service-token/rotate（R6 自动轮换，v0.1.132）：
// 认证 = 该 name 当前**active** 的 svc bearer 自身（无 admin 凭据）。
// 行为：旧行置 superseded + grace_until（24h 宽限，SvcGate 自动移出）→ 同 name/
// scopes 签新 token（新 jti，exp 重新计 90d）→ 返回新 bearer。
// 幂等护栏：宽限内的旧 bearer 再来 rotate → 409（SELECT 早退）；并发 rotate 由
// 条件 update 栅栏兜住（后到者 UPDATE 命中 0 行 → 同一 409）——任意时刻同一
// name 至多一行 active。
import { requireSvc, SVC_TTL_SECS, GRACE_SECS, nowSecs } from "../_shared";

export const schema = {
  description: "svc 凭据自轮换（需该 name 当前 active 的 svc bearer）",
};

async function post(): Promise<void> {
  const self = requireSvc();
  if (!self) { json.fail(403, "svc bearer required"); return; }
  const rows: any[] = await db.table("oj_svc_token")
    .select(["jti", "name", "scopes", "status"])
    .where({ field: "jti", op: "eq", value: self.jti })
    .all();
  const row = rows[0];
  if (!row) { json.fail(40311, "jti revoked or unknown"); return; } // fail-closed
  if (row.status && row.status !== "active") {
    json.fail(409, "bearer superseded — fetch the latest via GET current"); return;
  }
  let scopes: string[] = [];
  try { scopes = JSON.parse(String(row.scopes || "[]")); } catch { scopes = []; }
  const now = nowSecs();
  const jti = crypto.randomHex(16);
  const expiresAt = now + SVC_TTL_SECS;
  const token = await jwt.sign({
    sub: "svc:" + row.name,
    roles: ["bot"],
    claims: { svc: row.name, jti, scopes },
    ttl_secs: SVC_TTL_SECS,
  });
  // 并发栅栏：条件 update（仅 active/存量 NULL 行可翻转），凭影响行数判胜负。
  // 两个并发 rotate 持同一旧 bearer 时，行锁串行化两次 UPDATE——后到者重评估时
  // status 已被前者翻成 superseded，命中 0 行 → 409，从根上排除「两行 active」
  // （比 SELECT 校验可靠：SELECT 与 UPDATE 之间无锁，栅栏必须在写语句里）。
  // 无须 db.tx：insert 只跟随成功的翻转，其唯一现实故障（jti 撞 128bit 随机）
  // 可忽略；宽限窗保证旧 bearer 期内可重试恢复。
  const n = await db.table("oj_svc_token").update({
    status: "superseded",
    grace_until: now + GRACE_SECS,
  }).where({ and: [
    { field: "jti", op: "eq", value: self.jti },
    { or: [
      { field: "status", op: "isnull" },
      { field: "status", op: "eq", value: "active" },
    ] },
  ] }).run();
  if (!n) {
    json.fail(409, "bearer superseded — fetch the latest via GET current"); return;
  }
  await db.table("oj_svc_token").insert({
    jti, name: row.name, scopes: JSON.stringify(scopes),
    created_by: String(http.user!.id), created_at: now,
    status: "active", token, expires_at: expiresAt,
  }).run();
  json.ok({ token, jti, expires_at: expiresAt });
}

export default { post };
