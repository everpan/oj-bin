// R6 svc 凭据自动轮换集成测试（v0.1.132）：签发 → rotate → current → 幂等护栏。
// 前提：sample seed 已灌（demo/admin 账号 + admin 模块迁移）；default 库。
// 注意：SvcGate 正缓存 30s + miss 穿透（v0.1.132）——新签 jti 即时可见，用例内连续步骤无碍。

describe("svc token rotation (R6)", () => {
  // 名字带运行级唯一后缀：db_test 跨运行残留同名旧行（created_at 更大），
  // current 按 created_at desc 取 latest active 会取到上次运行的行。
  const svcName = "r6-test-bot-" + Date.now().toString(36);

  async function adminHdrs() {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    return { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
  }

  it("create → rotate → current → 旧 bearer 宽限后 409 → 链式续换", async () => {
    const h = await adminHdrs();
    // ① admin 签发（R6：响应带 expires_at，行落 status=active + token）
    const created = JSON.parse((await client.post("/admin/service-token/", {
      headers: h, body: JSON.stringify({ name: svcName, scopes: ["read"] }),
    })).body);
    expect(created.code).toBe(0);
    const old1 = created.data;
    expect(typeof old1.expires_at).toBe("number");

    const svcHdr = (t: string) => ({
      Authorization: "Bearer " + t, "X-TENANT-ID": "default",
    });

    // ② svc bearer 自轮换 → 新 bearer（新 jti）
    const r1 = JSON.parse((await client.post("/system/service-token/rotate/", {
      headers: svcHdr(old1.token), body: "{}",
    })).body);
    expect(r1.code).toBe(0);
    const new1 = r1.data;
    expect(new1.jti === old1.jti).toBeFalsy();
    expect(new1.expires_at >= old1.expires_at).toBeTruthy();

    // ③ 旧 bearer（宽限内）可 current 取回最新 active = 新 bearer
    const cur = JSON.parse((await client.get("/system/service-token/current/", {
      headers: svcHdr(old1.token),
    })).body);
    expect(cur.code).toBe(0);
    expect(cur.data.jti).toBe(new1.jti);
    expect(cur.data.token).toBe(new1.token);

    // ④ 旧 bearer 再 rotate → 409（幂等护栏：同一 name 至多一行 active）
    const again = JSON.parse((await client.post("/system/service-token/rotate/", {
      headers: svcHdr(old1.token), body: "{}",
    })).body);
    expect(again.code).toBe(409);

    // ⑤ 新 bearer 可继续链式续换
    const r2 = JSON.parse((await client.post("/system/service-token/rotate/", {
      headers: svcHdr(new1.token), body: "{}",
    })).body);
    expect(r2.code).toBe(0);
    expect(r2.data.jti === new1.jti).toBeFalsy();

    // ⑥ admin list 可见轮换状态（status 列；不带 token 明文）
    const list = JSON.parse((await client.get("/admin/service-token/", { headers: h })).body);
    const mine = list.data.filter((r: any) => r.name === svcName);
    expect(mine.length >= 3).toBeTruthy();
    expect(mine.some((r: any) => r.status === "superseded")).toBeTruthy();
    expect(mine.some((r: any) => r.status === "active")).toBeTruthy();
    expect(JSON.stringify(mine).includes(new1.token)).toBeFalsy();
  });

  it("并发 rotate：恰好一个成功、另一个 409；该 name 不产生两行 active", async () => {
    const h = await adminHdrs();
    const raceName = svcName + "-race";
    const created = JSON.parse((await client.post("/admin/service-token/", {
      headers: h, body: JSON.stringify({ name: raceName, scopes: ["read"] }),
    })).body);
    expect(created.code).toBe(0);
    const svcHdr = {
      Authorization: "Bearer " + created.data.token, "X-TENANT-ID": "default",
    };
    // 两个并发 rotate 持同一 active bearer：条件 UPDATE 栅栏按影响行数判胜负——
    // 后到者（或串行下第二个请求）得 409，任意时刻至多一行 active。
    const [r1, r2] = await Promise.all([
      client.post("/system/service-token/rotate/", { headers: svcHdr, body: "{}" }),
      client.post("/system/service-token/rotate/", { headers: svcHdr, body: "{}" }),
    ]);
    const p1 = JSON.parse(r1.body);
    const p2 = JSON.parse(r2.body);
    const codes = [p1.code, p2.code].sort((a: number, b: number) => a - b);
    expect(codes[0] === 0 && codes[1] === 409).toBeTruthy();
    const list = JSON.parse((await client.get("/admin/service-token/", { headers: h })).body);
    const mine = list.data.filter((r: any) => r.name === raceName);
    expect(mine.filter((r: any) => r.status === "active").length).toBe(1);
    expect(mine.filter((r: any) => r.status === "superseded").length).toBe(1);
  });

  it("匿名 → 401（守卫）；员工 token → 403（非 svc bearer）", async () => {
    const noAuth = JSON.parse((await client.post("/system/service-token/rotate/", {
      headers: { "X-TENANT-ID": "default" }, body: "{}",
    })).body);
    expect(noAuth.code).toBe(401); // 守卫层：未验签即拒
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const emp = JSON.parse((await client.post("/system/service-token/rotate/", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
      body: "{}",
    })).body);
    expect(emp.code).toBe(403); // handler 层：claims.svc 缺失
    const noAuthGet = JSON.parse((await client.get("/system/service-token/current/", {
      headers: { "X-TENANT-ID": "default" },
    })).body);
    expect(noAuthGet.code).toBe(401);
  });
});
