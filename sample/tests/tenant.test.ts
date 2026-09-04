// L1 多租户集成测试：tenant 启用后请求必须带 X-TENANT-ID（缺失/空 → 400）。
// 管线顺序：auth 检查在前、tenant 检查在后，故要观察到 400 需先通过 auth。

describe("tenant", () => {
  it("missing tenant header → 400", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const r = await client.get("/auth_demo/health", {
      headers: { Authorization: "Bearer " + token },
    });
    expect(r.status).toBe(400);
  });

  it("empty tenant header → 400", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const r = await client.get("/auth_demo/health", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "" },
    });
    expect(r.status).toBe(400);
  });

  it("valid tenant header → 200", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const r = await client.get("/auth_demo/health", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.data.status).toBe("ok");
  });
});
