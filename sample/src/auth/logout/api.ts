import { sessionKey } from "../_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    await kv.del(sessionKey(token));
    // oj-4 cookie 会话形态：两个 cookie 一起清（Max-Age=0；v0.1.46 起双 Set-Cookie
    // 同响应合法，oj_csrf 也是服务端签发的，一并作废）。
    json.header("Set-Cookie", "oj_sess=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    json.header("Set-Cookie", "oj_csrf=; SameSite=Lax; Path=/; Max-Age=0");
    json.ok(null);
  },
};
