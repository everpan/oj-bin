import { issueTokens } from "../../auth/_shared/session";
import { nowSecs } from "../../auth/_shared/util";

export default {
  async get() {
    const state = String(http.query.state ?? "");
    const key = "OJ-OIDC:STATE:" + state;
    // 一次一用：先 del 再判（CSRF/重放共用此闸）。重放必落空；并发窗口由 PKCE/state
    // 绑定兜底（redis 后端可换 GETDEL）。
    const raw = await kv.get(key);
    if (raw !== null) await kv.del(key);
    const snap = raw ? JSON.parse(raw) : null;
    if (!snap || !(snap.exp > nowSecs())) {
      json.fail(401, "invalid or expired state");
      return;
    }
    const code = String(http.query.code ?? "");
    if (!code) {
      json.fail(400, "code required");
      return;
    }
    // 网络错（DNS/拒连）与非 JSON 响应一律折叠进 502，不外抛（spec §7）。
    const dres = await fetch(snap.issuer + "/.well-known/openid-configuration").catch(() => null);
    const disc = dres && dres.ok ? await dres.json().catch(() => null) : null;
    if (!disc || !disc.token_endpoint || !disc.jwks_uri) {
      json.fail(502, "discovery failed");
      return;
    }
    const form = [
      "grant_type=authorization_code",
      "code=" + encodeURIComponent(code),
      "client_id=" + encodeURIComponent(snap.client_id),
      "client_secret=" + encodeURIComponent(snap.client_secret),
      "redirect_uri=" + encodeURIComponent(snap.redirect_uri),
      "code_verifier=" + snap.verifier,
    ].join("&");
    const tres = await fetch(disc.token_endpoint, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: form,
    }).catch(() => null);
    const tokens = tres && tres.ok ? await tres.json().catch(() => null) : null;
    if (!tokens || !tokens.id_token) {
      json.fail(502, "token endpoint failed");
      return;
    }
    const jres = await fetch(disc.jwks_uri).catch(() => null);
    const jwks = jres && jres.ok ? await jres.json().catch(() => null) : null;
    if (!jwks) {
      json.fail(502, "jwks fetch failed");
      return;
    }
    let claims: Record<string, unknown>;
    try {
      claims = oidc.verify(String(tokens.id_token), jwks);
    } catch {
      json.fail(401, "id_token verification failed");
      return;
    }
    if (claims.nonce !== snap.nonce || claims.iss !== snap.issuer) {
      json.fail(401, "id_token claims mismatch");
      return;
    }
    const auds = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
    if (!auds.includes(snap.client_id)) {
      json.fail(401, "id_token aud mismatch");
      return;
    }
    // JIT 本地映射：本地行 username 按 tenant+sub 命名空间隔离（全局 users.username
    // 若直接用 sub，多 IdP 会账号混淆/接管）。无则建（'!oidc' 非法占位 hash 不可密码
    // 登录——bcrypt.verify 对非法 hash 恒 false，零 schema 变更）。
    const sub = String(claims.sub);
    const localName = "oidc:" + snap.tenant + ":" + sub;
    let rows = await db.query("select id, roles from users where username = ?", [localName]);
    if (!rows.length) {
      await db.exec(
        "insert into users (username, password_hash, roles) values (?, ?, '[]')",
        [localName, "!oidc"],
      );
      rows = await db.query("select id, roles from users where username = ?", [localName]);
    }
    let roles: string[] = [];
    try {
      roles = JSON.parse(<string>rows[0].roles || "[]");
    } catch {
      roles = [];
    }
    json.ok(await issueTokens(String(rows[0].id), roles));
  },
};
