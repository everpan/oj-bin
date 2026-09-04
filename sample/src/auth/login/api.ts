import { issueTokens } from "../_shared/session";

export default {
  async post() {
    const body = http.body || {};
    const rows = await db.query(
      "select id, password_hash, roles from users where username = ?",
      [String(body.username ?? "")],
    );
    const row = rows[0];
    // 用户不存在与密码错同报（不泄露用户存在性）。
    if (!row || !(await bcrypt.verify(String(body.password ?? ""), row.password_hash || ""))) {
      json.fail(401, "invalid credentials");
      return;
    }
    // roles 列按 JSON 数组串解析，失败回落空。
    let roles: string[] = [];
    try { roles = JSON.parse(row.roles || "[]"); } catch { roles = []; }
    json.ok(await issueTokens(String(row.id), roles));
  },
};
