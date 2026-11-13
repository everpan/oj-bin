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
  // 任意时刻至多一行 active（rotate/admin 签发的驱旧栅栏保证；存量行 status 为
  // NULL 视为 active）。created_at 同秒平局时按 jti **字节序**取定（评审 S5：
  // localeCompare 是 ICU 区域相关排序，跨运行环境结果可漂移；字节序与引擎/地域
  // 无关）——排序确定性，不随引擎返回序漂移。
  const actives = rows
    .filter((r: any) => !r.status || r.status === "active")
    .sort((a: any, b: any) => {
      const aj = String(a.jti), bj = String(b.jti);
      return Number(b.created_at) - Number(a.created_at) ||
        (aj < bj ? -1 : aj > bj ? 1 : 0);
    });
  const row = actives[0];
  if (!row || !row.token) {
    json.fail(404, "no active token for this svc name"); return;
  }
  // 明文面只回**当前 active 行持有者**（评审 #4）：SvcGate 放行宽限内旧 jti 是
  // 为在途请求平滑，不是恢复通道——泄漏的旧 bearer 若可 current 取回新明文，即可
  // 经 current→rotate 链式续换，轮换=吊销被架空（只有 grace 过期/admin DELETE
  // 才真吊销）。宽限 bearer 取明文 → 40311（与吊销同码，admin 重签才是正路）。
  if (String(row.jti) !== self.jti) {
    json.fail(40311, "bearer superseded — only the current active bearer can read the token");
    return;
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
