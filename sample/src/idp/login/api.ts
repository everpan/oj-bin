import { nowSecs } from "../../auth/_shared/util";

export default {
  async post() {
    const b = http.body || {};
    const rows = await db.query(
      "select id, password_hash from users where username = ?",
      [String(b.username ?? "")],
    );
    const row = rows[0];
    // 用户不存在与密码错同报（不泄露用户存在性，对齐 auth/login）。
    if (!row || !(await bcrypt.verify(String(b.password ?? ""), <string>row.password_hash || ""))) {
      json.fail(401, "invalid credentials");
      return;
    }
    const sid = crypto.randomHex(32);
    const ttl = jwt.refreshDuration;
    await kv.set(
      "OJ-IDP:SESS:" + sid,
      JSON.stringify({ uid: String(row.id), exp: nowSecs() + ttl }),
    );
    await kv.expire("OJ-IDP:SESS:" + sid, ttl);
    // Cookie Path 取 issuer 的路径段（登录会话只在 OP 端点内可见）。
    const path = oidc.issuer.replace(/^https?:\/\/[^/]+/, "");
    json.header("Set-Cookie", `IDP_SESSION=${sid}; HttpOnly; Path=${path}; Max-Age=${ttl}`);
    json.ok({ uid: String(row.id) });
  },
};
