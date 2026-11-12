// GET /v1/api/system/service-token/current（R6 自动轮换，v0.1.132）：
// 认证 = 该 name 当前有效（active 或宽限内）的 svc bearer 自身。
// 返回该 name 最新 active 行的 bearer——消费方进程重启后恢复最新令牌，
// 避免重新人工配置（「惰性自愈轮换」的恢复端）。
import { requireSvc } from "../_shared";

export const schema = {
  description: "取该 svc name 当前最新有效 bearer（svc 自认证，跨重启恢复面）",
};

async function get(): Promise<void> {
  const self = requireSvc();
  if (!self) { json.fail(403, "svc bearer required"); return; }
  const rows: any[] = await db.table("oj_svc_token")
    .select(["jti", "status", "scopes", "token", "created_at", "expires_at"])
    .where({ field: "name", op: "eq", value: self.name })
    .all();
  // 任意时刻至多一行 active（rotate 条件 update 栅栏保证）；存量行 status 为 NULL
  // 视为 active。created_at 同秒平局时按 jti 字典序取定——排序确定性，不随引擎
  // 返回序漂移（存量多 active 行的保守行为，rotate 路径本身不产生平局）。
  const actives = rows
    .filter((r: any) => !r.status || r.status === "active")
    .sort((a: any, b: any) =>
      Number(b.created_at) - Number(a.created_at) ||
      String(a.jti).localeCompare(String(b.jti)));
  const row = actives[0];
  if (!row || !row.token) {
    json.fail(404, "no active token for this svc name"); return;
  }
  let scopes: string[] = [];
  try { scopes = JSON.parse(String(row.scopes || "[]")); } catch { scopes = []; }
  json.ok({
    token: row.token, // 明文面：仅 svc 自认证端点回显（admin list 永不选择该列）
    jti: row.jti,
    expires_at: Number(row.expires_at),
    scopes,
  });
}

export default { get };
