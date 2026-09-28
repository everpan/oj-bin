import { sessionKey } from "../_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    await kv.del(sessionKey(token));
    // oj-4 cookie 会话形态：清两个 cookie（Max-Age=0）。csrf cookie 是前端
    // document.cookie 写的，服务端只管清 session 侧 + 提示前端清 csrf。
    json.header("Set-Cookie", "oj_sess=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    json.ok({ csrf_cleared: true });
  },
};
