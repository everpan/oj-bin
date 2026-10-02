import { issueTokens } from "../_shared/session";

export default {
  async post() {
    const body = http.body || {};
    const rows = await db.table("users")
      .select(["id", "password_hash", "roles"])
      .where({ field: "username", op: "eq", value: String(body.username ?? "") })
      .all();
    const row = rows[0];
    // 用户不存在与密码错同报（不泄露用户存在性）。
    if (!row || !(await bcrypt.verify(String(body.password ?? ""), String(row.password_hash || "")))) {
      json.fail(401, "invalid credentials");
      return;
    }
    // roles 列按 JSON 数组串解析，失败回落空。
    let roles: string[] = [];
    try { roles = JSON.parse(String(row.roles || "[]")); } catch { roles = []; }
    const tokens = await issueTokens(String(row.id), roles);
    // oj-4 cookie 会话形态（oj-auth cfg cookie.enabled 时守卫侧生效）：
    // HttpOnly 的 session cookie（值 = 与 Bearer 同 secret 的 access JWT）+
    // CSRF 双提交 cookie 同响应双发（v0.1.46 json.header 同名头可重复，Set-Cookie
    // 合法落多个值；oj_csrf 非 HttpOnly——双提交要 JS 读得到）。
    const maxAge = 86400; // 与 oj-auth cookie.ttl_secs 默认对齐
    json.header(
      "Set-Cookie",
      `oj_sess=${tokens.access_token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=${maxAge}`,
    );
    json.header(
      "Set-Cookie",
      `oj_csrf=${crypto.randomHex(16)}; SameSite=Lax; Path=/; Max-Age=${maxAge}`,
    );
    json.ok(tokens);
  },
};
