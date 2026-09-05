import { b64uFromHex, nowSecs, parseForm } from "../../auth/_shared/util";

const TOKEN_TTL = 3600;

export default {
  async post() {
    const f = parseForm(http.body);
    if (f.grant_type !== "authorization_code") {
      json.fail(400, "unsupported grant_type");
      return;
    }
    const clientCfg = (oidc.clients || {})[String(f.client_id ?? "")];
    if (!clientCfg || clientCfg.secret !== String(f.client_secret ?? "")) {
      json.fail(401, "invalid client");
      return;
    }
    const key = "OJ-OIDC:CODE:" + String(f.code ?? "");
    // 一次一用：先 del 再判。重放必落空；并发窗口由 PKCE/state 绑定兜底（redis 后端可换 GETDEL）。
    const raw = await kv.get(key);
    if (raw !== null) await kv.del(key);
    const code = raw ? JSON.parse(raw) : null;
    if (!code || !(code.exp > nowSecs())) {
      json.fail(401, "invalid or expired code");
      return;
    }
    if (code.redirect_uri !== f.redirect_uri) {
      json.fail(400, "redirect_uri mismatch");
      return;
    }
    if (code.client_id !== f.client_id) {
      json.fail(401, "code was issued to another client");
      return;
    }
    // PKCE：S256(verifier) == 挑战。
    if (b64uFromHex(crypto.sha256Hex(String(f.code_verifier ?? ""))) !== code.challenge) {
      json.fail(401, "pkce verification failed");
      return;
    }
    const now = nowSecs();
    const claims = {
      iss: oidc.issuer,
      sub: code.uid,
      aud: code.client_id,
      iat: now,
      exp: now + TOKEN_TTL,
      nonce: code.nonce,
      tenant: code.tenant,
    };
    json.raw({
      access_token: oidc.sign(claims),
      id_token: oidc.sign(claims),
      token_type: "Bearer",
      expires_in: TOKEN_TTL,
    });
  },
};
