import { b64uFromHex, nowSecs, redirect } from "../../auth/_shared/util";

const STATE_TTL = 600;

// 回跳地址：从请求 Host 头拼（RP 侧零配置；OP 白名单侧显式配置才安全）。
// ponytail: base 硬编码 /v1/api（demo 配置固定）；server.base 可配时这里要跟着改。
function redirectUri(): string {
  const host = String(http.headers.host ?? "localhost:9778");
  const proto = host.startsWith("localhost") || host.startsWith("127.") ? "http" : "https";
  return `${proto}://${host}/v1/api/oidc/callback`;
}

export default {
  async get() {
    const tenant = String(http.query.tenant ?? "");
    const rp = (oidc.rp || {})[tenant];
    if (!rp) {
      json.fail(400, "unknown tenant");
      return;
    }
    // 通用 discovery：endpoints 不硬编码（对接任意标准 IdP）。
    const res = await fetch(rp.issuer + "/.well-known/openid-configuration");
    const disc = res.ok ? await res.json() : null;
    if (!disc || !disc.authorization_endpoint) {
      json.fail(502, "discovery failed");
      return;
    }
    const state = crypto.randomHex();
    const verifier = crypto.randomHex(32);
    const nonce = crypto.randomHex(16);
    await kv.set(
      "OJ-OIDC:STATE:" + state,
      JSON.stringify({
        tenant,
        nonce,
        verifier,
        issuer: rp.issuer,
        client_id: rp.client_id,
        client_secret: rp.client_secret,
        redirect_uri: redirectUri(),
        exp: nowSecs() + STATE_TTL,
      }),
    );
    await kv.expire("OJ-OIDC:STATE:" + state, STATE_TTL);
    const q = [
      "response_type=code",
      "client_id=" + encodeURIComponent(rp.client_id),
      "redirect_uri=" + encodeURIComponent(redirectUri()),
      "scope=" + encodeURIComponent(rp.scope),
      "state=" + state,
      "nonce=" + nonce,
      "code_challenge=" + b64uFromHex(crypto.sha256Hex(verifier)),
      "code_challenge_method=S256",
    ].join("&");
    redirect(disc.authorization_endpoint + "?" + q);
  },
};
