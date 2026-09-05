// L1 OP 骨架测试：discovery / jwks 对外裸 JSON（json.raw），标准 OIDC 客户端可直接消费。
// 运行：oj test -c sample/config.yaml -d sample/src -t tests

import { b64uFromHex } from "../src/auth/_shared/util";

function headerOf(r: { headers: Record<string, string> }, name: string): string {
  const k = Object.keys(r.headers).find((h) => h.toLowerCase() === name.toLowerCase());
  return k === undefined ? "" : String(r.headers[k]);
}

// RP（Task 8）：tenant 路由 + discovery + state/nonce/PKCE。302 跳转腿在进程内不可达
// （oj test 无 TCP listener，fetch 自身 discovery 必失败），完整链路归 Rust e2e（Task 10）。
describe("oidc login (RP)", () => {
  it("rejects unknown tenant", async () => {
    const r = await client.get("/oidc/login?tenant=nope");
    expect(r.status).toBe(400);
    expect(JSON.parse(r.body).msg).toBe("unknown tenant");
  });
});

describe("idp discovery/jwks", () => {
  it("exposes bare discovery document with RS256 + code", async () => {
    const r = await client.get("/idp/.well-known/openid-configuration");
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.issuer).toContain("/v1/api/idp");
    expect(body.response_types_supported[0]).toBe("code");
    expect(body.id_token_signing_alg_values_supported[0]).toBe("RS256");
    expect(body.jwks_uri).toContain("/idp/jwks.json");
  });

  it("exposes bare RSA JWKS with 16-char kid", async () => {
    const r = await client.get("/idp/jwks.json");
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.keys[0].kty).toBe("RSA");
    expect(body.keys[0].alg).toBe("RS256");
    expect(body.keys[0].kid.length).toBe(16);
  });
});

async function idpCookie(): Promise<string> {
  // op_client_dispatch 的 body 是 string（op #[string] 契约），须 JSON.stringify。
  const r = await client.post("/idp/login", {
    body: JSON.stringify({ username: "demo", password: "demo1234" }),
  });
  expect(r.status).toBe(200);
  const setCookie = headerOf(r, "set-cookie");
  expect(setCookie).toContain("IDP_SESSION=");
  return setCookie.split(";")[0];
}

describe("idp login/authorize", () => {
  it("login rejects bad credentials without leaking existence", async () => {
    const r = await client.post("/idp/login", {
      body: JSON.stringify({ username: "demo", password: "wrong" }),
    });
    expect(r.status).toBe(401);
    expect(JSON.parse(r.body).msg).toBe("invalid credentials");
  });

  it("authorize enforces whitelist, PKCE and session; issues one-time code", async () => {
    const noSess = await client.get("/idp/authorize?response_type=code&client_id=sample-rp&scope=openid");
    expect(noSess.status).toBe(401);
    expect(JSON.parse(noSess.body).msg).toBe("login required");
    const cookie = await idpCookie();
    const base =
      "/idp/authorize?response_type=code&client_id=sample-rp" +
      "&redirect_uri=" +
      encodeURIComponent("http://localhost:9778/v1/api/oidc/callback") +
      "&scope=openid&state=st1&nonce=n1&code_challenge=" +
      "x".repeat(43) +
      "&code_challenge_method=S256";
    const badClient = await client.get(base.replace("sample-rp", "nope"), { headers: { Cookie: cookie } });
    expect(badClient.status).toBe(400);
    const badUri = await client.get(
      base.replace(encodeURIComponent("http://localhost:9778/v1/api/oidc/callback"), encodeURIComponent("http://evil/cb")),
      { headers: { Cookie: cookie } },
    );
    expect(badUri.status).toBe(400);
    const noPkce = await client.get(base.split("&code_challenge")[0], { headers: { Cookie: cookie } });
    expect(noPkce.status).toBe(400);
    const ok = await client.get(base, { headers: { Cookie: cookie } });
    expect(ok.status).toBe(302);
    const loc = headerOf(ok, "location");
    expect(loc).toContain("code=");
    expect(loc).toContain("state=st1");
  });
});

describe("idp token/userinfo (full code flow in-process)", () => {
  it("exchanges code for RS256 tokens; replay rejected; userinfo reads sub/tenant", async () => {
    const verifier = crypto.randomHex(32);
    const challenge = b64uFromHex(crypto.sha256Hex(verifier));
    const cookie = await idpCookie();
    const cb = encodeURIComponent("http://localhost:9778/v1/api/oidc/callback");
    const ar = await client.get(
      `/idp/authorize?response_type=code&client_id=sample-rp&redirect_uri=${cb}` +
        `&scope=openid&state=st2&nonce=n2&code_challenge=${challenge}&code_challenge_method=S256`,
      { headers: { Cookie: cookie } },
    );
    expect(ar.status).toBe(302);
    const loc = headerOf(ar, "location");
    const code = loc.split("code=")[1].split("&")[0];
    const tokenBody =
      `grant_type=authorization_code&code=${code}&client_id=sample-rp` +
      `&client_secret=rp-secret&redirect_uri=${cb}&code_verifier=${verifier}`;
    const tr = await client.post("/idp/token", {
      body: tokenBody,
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
    });
    expect(tr.status).toBe(200);
    const tok = JSON.parse(tr.body); // 裸 JSON（json.raw）
    expect(tok.access_token).toBeTruthy();
    expect(tok.id_token).toBeTruthy();
    expect(tok.token_type).toBe("Bearer");
    // code 一次一用：重放 401。
    const replay = await client.post("/idp/token", {
      body: tokenBody,
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
    });
    expect(replay.status).toBe(401);
    // PKCE 错 verifier → 401（拿新 code 试）。
    // userinfo：access_token 验签后读 sub/tenant。
    const ui = await client.get("/idp/userinfo", {
      headers: { Authorization: "Bearer " + tok.access_token },
    });
    expect(ui.status).toBe(200);
    const u = JSON.parse(ui.body);
    expect(String(u.sub)).toBeTruthy();
    expect(u.tenant).toBe("default");
    const bad = await client.get("/idp/userinfo", { headers: { Authorization: "Bearer junk" } });
    expect(bad.status).toBe(401);
  });

  it("rejects wrong PKCE verifier, redirect_uri mismatch and bad client secret", async () => {
    const newCode = async () => {
      const verifier = crypto.randomHex(32);
      const challenge = b64uFromHex(crypto.sha256Hex(verifier));
      const cookie = await idpCookie();
      const cb = encodeURIComponent("http://localhost:9778/v1/api/oidc/callback");
      const ar = await client.get(
        `/idp/authorize?response_type=code&client_id=sample-rp&redirect_uri=${cb}` +
          `&scope=openid&state=s&nonce=n&code_challenge=${challenge}&code_challenge_method=S256`,
        { headers: { Cookie: cookie } },
      );
      expect(ar.status).toBe(302);
      return headerOf(ar, "location").split("code=")[1].split("&")[0];
    };
    const cb = encodeURIComponent("http://localhost:9778/v1/api/oidc/callback");
    const post = (code: string, extra: string) =>
      client.post("/idp/token", {
        body: `grant_type=authorization_code&code=${code}&client_id=sample-rp&redirect_uri=${cb}${extra}`,
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
      });
    expect((await post(await newCode(), "&client_secret=rp-secret&code_verifier=wrong")).status).toBe(401);
    expect(
      (
        await post(
          await newCode(),
          "&client_secret=rp-secret&redirect_uri=" + encodeURIComponent("http://evil/cb") + "&code_verifier=whatever",
        )
      ).status,
    ).toBe(400);

    expect(
      (await post(await newCode(), "&client_secret=nope&code_verifier=whatever")).status,
    ).toBe(401);
  });
});
