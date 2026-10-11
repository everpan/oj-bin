// CAS 2.0 回跳腿：ticket → serviceValidate XML 判定 → JIT → 双 cookie 交接 → 302 回前端。
// 红线：token 不进 URL（httpOnly 双 cookie）；302 目标走 safeNext 白名单（防 open-redirect）；
// service 与跳转腿同串（装配期 callback_url）。
import { issueTokens } from "#/auth/_shared/session";
import { redirect } from "#/auth/_shared/util";
import { jit, providerOf, safeNext, setSessionCookies } from "../../_shared/core";

export default {
  async get() {
    const name = String(http.query.provider ?? "");
    const p = providerOf(name);
    if (!p || p.kind !== "cas2") { json.fail(400, `unknown cas provider: ${name}`); return; }
    const ticket = String(http.query.ticket ?? "");
    let r: Response;
    try {
      r = await fetch(`${p.cas_validate_url}?ticket=${encodeURIComponent(ticket)
        }&service=${encodeURIComponent(p.callback_url ?? "")}`);
    } catch {
      json.fail(502, "sso upstream failed");
      return;
    }
    const xml = await r.text();
    // 命名空间前缀可变（<cas:…> 或默认命名空间）、元素可跨行：[\s\S] 而非 .
    const ok = /<[\w.-]*:?authenticationSuccess[\s/>]/.test(xml);
    const m = xml.match(/<(?:[\w.-]+:)?user(?:\s[^>]*)?>([\s\S]*?)<\/(?:[\w.-]+:)?user>/);
    if (!r.ok || !ok || !m || !m[1].trim()) {
      json.fail(401, "invalid credentials");
      return;
    }
    const u = await jit(name, m[1].trim(), xml, p);
    const tokens = await issueTokens(u.id, u.roles);
    setSessionCookies(tokens.access_token);
    redirect(safeNext(http.query.next));
  },
};
