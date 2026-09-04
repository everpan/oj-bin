// L1 鉴权集成测试：真实 deno_core 运行时 + 进程内 oneshot 派发（零 TCP）。
// 运行：oj test -c sample/config.yaml -d sample/src
//
// 注意：config 启用 auth（匿名仅 "/health" 与 /auth/* 三项）与 tenant（X-TENANT-ID 必填）。
// 受保护路由需 Bearer；所有非 /v1/api/blob/* 路由均需 X-TENANT-ID。

describe("auth", () => {
  it("login with valid credentials returns access_token", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    expect(token).toBeTruthy();
  });

  it("login with wrong password → 401", async () => {
    const r = await client.post("/auth/login", {
      headers: { "X-TENANT-ID": "default" },
      body: JSON.stringify({ username: "demo", password: "wrong" }),
    });
    expect(r.status).toBe(401);
  });

  it("refresh rotates and old token is single-use", async () => {
    const H = { "X-TENANT-ID": "default" };
    const r1 = await client.post("/auth/login", {
      headers: H, body: JSON.stringify({ username: "demo", password: "demo1234" }),
    });
    const rt1 = JSON.parse(r1.body).data.refresh_token;
    const r2 = await client.post("/auth/refresh", {
      headers: H, body: JSON.stringify({ refresh_token: rt1 }),
    });
    expect(r2.status).toBe(200);
    const r3 = await client.post("/auth/refresh", {
      headers: H, body: JSON.stringify({ refresh_token: rt1 }),
    });
    expect(r3.status).toBe(401);
  });

  it("logout kills the session", async () => {
    const H = { "X-TENANT-ID": "default" };
    const r1 = await client.post("/auth/login", {
      headers: H, body: JSON.stringify({ username: "demo", password: "demo1234" }),
    });
    const rt = JSON.parse(r1.body).data.refresh_token;
    await client.post("/auth/logout", { headers: H, body: JSON.stringify({ refresh_token: rt }) });
    const r2 = await client.post("/auth/refresh", {
      headers: H, body: JSON.stringify({ refresh_token: rt }),
    });
    expect(r2.status).toBe(401);
  });

  it("protected /auth_demo/me without token → 401", async () => {
    const r = await client.get("/auth_demo/me", {
      headers: { "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(401);
  });

  it("protected /auth_demo/me with token → 200 + user", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const r = await client.get("/auth_demo/me", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.user).toBeTruthy();
    expect(body.data.user.id).toBe("1");
  });
});
