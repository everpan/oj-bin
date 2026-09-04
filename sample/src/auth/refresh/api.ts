import { issueTokens, nowSecs, sessionKey } from "../_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    const key = sessionKey(token);
    const raw = await kv.get(key);
    const sess = raw ? JSON.parse(raw) : null;
    if (!sess || !(sess.exp > nowSecs())) {
      json.fail(401, "invalid or expired refresh token");
      return;
    }
    // session 只存 uid——roles 重查库取最新。
    const rows = await db.query("select roles from users where id = ?", [sess.uid]);
    let roles: string[] = [];
    try { roles = JSON.parse((rows[0] || {}).roles || "[]"); } catch { roles = []; }
    // 轮换：先删旧 session（旧 refresh 立即失效，一次一用）再签新对。
    await kv.del(key);
    json.ok(await issueTokens(String(sess.uid), roles));
  },
};
