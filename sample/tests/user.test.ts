// L1 用户账号集成测试：覆盖 list / create / 校验失败三类路径。

describe("user account", () => {
  it("lists accounts (auth + tenant) → 200 array", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(Array.isArray(body.data)).toBeTruthy();
  });

  it("creates an account via POST → 200 created", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.post("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
      body: JSON.stringify({ name: "tank", role: "user" }),
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.data.created).toBeTruthy();
  });

  it("rejects invalid role → 400", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.post("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
      body: JSON.stringify({ name: "x", role: "king" }),
    });
    expect(r.status).toBe(400);
  });

  it("OPTIONS reports supported methods → 200", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.options("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.data.methods).toContain("post");
  });
});
