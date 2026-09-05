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
