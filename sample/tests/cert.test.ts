// cert 模块 L1 集成测试：admin 生成/备注/renew/删除全链路；user 角色 403。
// 运行：oj test -c sample/config.yaml -d sample/src

const ADMIN = { Authorization: "", "X-TENANT-ID": "default" };
const USER = { Authorization: "", "X-TENANT-ID": "default" };

describe("cert", () => {
  beforeEach(async () => {
    ADMIN.Authorization = "Bearer " + (await client.login("demo", "demo1234"));
    USER.Authorization = "Bearer " + (await client.login("trinity", "demo1234"));
  });

  it("user role gets 403 on create", async () => {
    const r = await client.post("/cert/", {
      headers: USER,
      body: JSON.stringify({ name: "nope" }),
    });
    expect(r.status).toBe(403);
  });

  it("admin create → list valid → note → renew → delete", async () => {
    // 创建
    const created = await client.post("/cert/", {
      headers: ADMIN,
      body: JSON.stringify({ name: "it-cert", note: "first", days: 30 }),
    });
    expect(created.status).toBe(200);
    const createdBody = JSON.parse(created.body);
    expect(createdBody.code).toBe(0);
    const id = createdBody.data.id;
    expect(id).toBeTruthy();

    // 列表：可见且 status=valid、不含 private_pem
    const list = JSON.parse((await client.get("/cert/", { headers: ADMIN })).body);
    const row = list.data.find((x: any) => x.id === id);
    expect(row.status).toBe("valid");
    expect(row.note).toBe("first");
    expect(row.private_pem).toBeFalsy();

    // 改备注
    const patched = await client.patch("/cert/item/", {
      headers: ADMIN,
      body: JSON.stringify({ id, note: "updated note" }),
    });
    expect(patched.status).toBe(200);

    // 详情（含私钥）→ renew → exp 增长、公钥不变、备注保留
    const before = JSON.parse(
      (await client.get("/cert/item/?id=" + id, { headers: ADMIN })).body,
    ).data;
    expect(before.private_pem).toBeTruthy();
    const renewed = await client.post("/cert/renew/", {
      headers: ADMIN,
      body: JSON.stringify({ id, days: 365 }),
    });
    expect(renewed.status).toBe(200);
    const after = JSON.parse(
      (await client.get("/cert/item/?id=" + id, { headers: ADMIN })).body,
    ).data;
    // ponytail: 测试运行时 expect 仅支持 toBe/toEqual/toBeTruthy/toBeFalsy/toContain
    expect(after.exp > before.exp).toBe(true);
    expect(after.public_pem).toBe(before.public_pem);
    expect(after.cert_jws === before.cert_jws).toBe(false);
    expect(after.note).toBe("updated note");

    // 删除 → 404
    expect((await client.del("/cert/item/?id=" + id, { headers: ADMIN })).status).toBe(200);
    const gone = await client.get("/cert/item/?id=" + id, { headers: ADMIN });
    expect(gone.status).toBe(404);

    // 参数校验：days 越界 → 400
    const bad = await client.post("/cert/", {
      headers: ADMIN,
      body: JSON.stringify({ name: "bad", days: 0 }),
    });
    expect(bad.status).toBe(400);
  });
});
