// L1 OP 骨架测试：discovery / jwks 对外裸 JSON（json.raw），标准 OIDC 客户端可直接消费。
// 运行：oj test -c sample/config.yaml -d sample/src -t tests

function headerOf(r: { headers: Record<string, string> }, name: string): string {
  const k = Object.keys(r.headers).find((h) => h.toLowerCase() === name.toLowerCase());
  return k === undefined ? "" : String(r.headers[k]);
}

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
