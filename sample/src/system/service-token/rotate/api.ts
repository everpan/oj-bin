// POST /v1/api/system/service-token/rotate（R6 自动轮换，v0.1.132）：
// 认证 = 该 name 当前**active** 的 svc bearer 自身（无 admin 凭据）。
// 行为：旧行置 superseded + grace_until（24h 宽限，SvcGate 自动移出）→ 同 name/
// scopes 签新 token（新 jti，exp 重新计 90d）→ 返回新 bearer。
// 幂等护栏：宽限内的旧 bearer 再来 rotate → 409（SELECT 早退）；并发 rotate 由
// 条件 update 栅栏兜住（后到者 UPDATE 命中 0 行 → 同一 409）——任意时刻同一
// name 至多一行 active。
// 原子性（db.tx，评审修复）：supersede + insert 同事务——insert 失败回滚翻转，
// 旧 bearer 仍 active，宽限窗内重试恢复的承诺成立（非原子实现下 insert 失败
// 会使该 name 永久无 active 行：重试撞 409、GET current 404，svc 账号卡死）。
import { requireSvc, SVC_TTL_SECS, GRACE_SECS, nowSecs } from "../_shared";
// 40311 唯一源在 auth/_shared/obo（评审清理项：裸字面量两处写必漂移）。
import { OBO } from "../../../auth/_shared/obo";

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
  if (!row) { json.fail(OBO.REVOKED, "jti revoked or unknown"); return; } // fail-closed
  if (row.status && row.status !== "active") {
    json.fail(409, "bearer superseded — fetch the latest via GET current"); return;
  }
  let scopes: string[] = [];
  try { scopes = JSON.parse(String(row.scopes || "[]")); } catch { scopes = []; }
  const now = nowSecs();
  const jti = crypto.randomHex(16);
  const expiresAt = now + SVC_TTL_SECS;
  // 事务外签发（不触库）；jwt.sign 失败无副作用。
  const token = await jwt.sign({
    sub: "svc:" + row.name,
    roles: ["bot"],
    claims: { svc: row.name, jti, scopes },
    ttl_secs: SVC_TTL_SECS,
  });
  // 并发栅栏在事务内：条件 update（仅 active/存量 NULL 行可翻转），凭影响行数
  // 判胜负——两个并发 rotate 持同一旧 bearer 时行锁串行化，后到者 UPDATE 命中
  // 0 行 → 409，从根上排除「两行 active」。insert 与翻转同事务：任一失败整事务
  // 回滚（insert 唯一现实故障 jti 撞 128bit 随机也不再留下 superseded 残行）。
  let flipped = true;
  try {
    await db.tx(async (tx: any) => {
      const n = await tx.table("oj_svc_token").update({
        status: "superseded",
        grace_until: now + GRACE_SECS,
      }).where({ and: [
        { field: "jti", op: "eq", value: self.jti },
        { or: [
          { field: "status", op: "isnull" },
          { field: "status", op: "eq", value: "active" },
        ] },
      ] }).run();
      if (!n) { flipped = false; throw new Error("bearer superseded"); }
      await tx.table("oj_svc_token").insert({
        jti, name: row.name, scopes: JSON.stringify(scopes),
        created_by: String(http.user!.id), created_at: now,
        status: "active", token, expires_at: expiresAt,
      }).run();
    });
  } catch (e) {
    if (!flipped) {
      json.fail(409, "bearer superseded — fetch the latest via GET current"); return;
    }
    throw e; // 真故障：事务已回滚，旧 bearer 仍 active，宽限窗内可重试
  }
  json.ok({ token, jti, expires_at: expiresAt });
}

export default { post };
