import { nowSecs, parseCookies, redirect } from "../../auth/_shared/util";

const CODE_TTL = 60;

export default {
  async get() {
    const q = http.query;
    // 会话门禁前置：未登录一律 401 login required（登录后才谈 client 校验；
    // 白名单不命中时依旧绝不重定向——redirect_uri 未验证前跳转 = 开放重定向）。
    const cookies = parseCookies(String(http.headers.cookie ?? ""));
    const sessRaw = await kv.get("OJ-IDP:SESS:" + (cookies.IDP_SESSION ?? ""));
    const sess = sessRaw ? JSON.parse(sessRaw) : null;
    if (!sess || !(sess.exp > nowSecs())) {
      json.fail(401, "login required");
      return;
    }
    const clientCfg = (oidc.clients || {})[String(q.client_id ?? "")];
    if (!clientCfg) {
      json.fail(400, "unknown client");
      return;
    }
    if (!(clientCfg.redirect_uris || []).includes(String(q.redirect_uri ?? ""))) {
      json.fail(400, "redirect_uri not registered");
      return;
    }
    if (String(q.response_type ?? "") !== "code") {
      json.fail(400, "response_type must be code");
      return;
    }
    if (!String(q.scope ?? "").split(" ").includes("openid")) {
      json.fail(400, "scope must include openid");
      return;
    }
    const challenge = String(q.code_challenge ?? "");
    if (String(q.code_challenge_method ?? "") !== "S256" || challenge.length < 43) {
      json.fail(400, "PKCE S256 required");
      return;
    }
    const code = crypto.randomHex(32);
    await kv.set(
      "OJ-OIDC:CODE:" + code,
      JSON.stringify({
        client_id: q.client_id,
        redirect_uri: q.redirect_uri,
        state: q.state ?? "",
        nonce: q.nonce ?? "",
        challenge,
        uid: sess.uid,
        tenant: clientCfg.tenant,
        exp: nowSecs() + CODE_TTL,
      }),
    );
    await kv.expire("OJ-OIDC:CODE:" + code, CODE_TTL);
    redirect(
      `${q.redirect_uri}?code=${code}&state=${encodeURIComponent(String(q.state ?? ""))}`,
    );
  },
};
