// HTTP 表单 SSO：转发凭据到企业统一登录端点，success_when 判定 → JIT → 签发
// （双 cookie 同构 auth/login，场景 14）。本端点是普通业务路由，**必须匿名**
// （config auth.anonymous_paths / tenant.anonymous_paths——无 token 可言）。
// 两态拆分：网络错/上游非 2xx → 502；判定失败 → 401；不回吐外部响应体。
import { issueTokens } from "#/auth/_shared/session";
import { hitWhen, jit, providerOf } from "../_shared/core";

export default {
  async post() {
    const name = String(http.body?.provider ?? "");
    const p = providerOf(name);
    if (!p) { json.fail(400, `unknown sso provider: ${name}`); return; }
    const username = String(http.body?.username ?? "");
    const fields: Record<string, unknown> = { ...(p.extra_fields ?? {}) };
    fields[p.user_field ?? "username"] = username;
    fields[p.pass_field ?? "password"] = String(http.body?.password ?? "");
    let r: OjFetchResponse;
    try {
      r = await fetch(p.login_url, {
        method: "POST",
        headers: {
          "content-type": p.body_encoding === "json"
            ? "application/json"
            : "application/x-www-form-urlencoded",   // 缺省 form：对齐常见企业 SSO 上游
        },
        body: p.body_encoding === "json"
          ? JSON.stringify(fields)
          : new URLSearchParams(fields as Record<string, string>).toString(),
      });
    } catch {
      json.fail(502, "sso upstream failed");
      return;
    }
    // 判定用 body：text + JSON.parse（兼容上游直接回对象或 json.raw(string) 双重编码形态）
    let body: unknown = null;
    try {
      body = JSON.parse(await r.text());
      if (typeof body === "string") body = JSON.parse(body);
    } catch {
      body = null;
    }
    if (!r.ok || !body || !hitWhen(body, p.success_when ?? {})) {
      json.fail(401, "invalid credentials");
      return;
    }
    const u = await jit(name, username, body, p);
    json.ok(await issueTokens(u.id, u.roles));
  },
};
