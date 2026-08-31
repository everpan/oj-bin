// L1 admin 模块集成测试：react-antd-admin 接口移植的端到端契约（auth + tenant 全开）。

describe("admin role", () => {
  it("GET /role-list → 分页包装 + 种子角色（camelCase）", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/role-list", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.total).toBe(2);
    expect(body.data.pageSize).toBe(10);
    expect(body.data.current).toBe(1);
    expect(body.data.list[0].code).toBe("admin");
    expect(body.data.list[0].createTime).toBe(1729752330782);
  });

  it("GET /role-list 过滤 name → 只剩匹配项", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/role-list?name=" + encodeURIComponent("普通"), {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.data.total).toBe(1);
    expect(body.data.list[0].code).toBe("common");
  });

  it("role-item POST→PUT→DELETE 闭环（DELETE body 为裸数字）", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const created = JSON.parse((await client.post("/role-item", {
      headers: h, body: JSON.stringify({ name: "运营", code: "ops", status: 1, remark: "r" }),
    })).body);
    expect(created.code).toBe(0);
    const id = created.data.id;
    expect(id > 0).toBeTruthy();

    const updated = JSON.parse((await client.put("/role-item", {
      headers: h, body: JSON.stringify({ id, name: "运营2", code: "ops", status: 0, remark: "r2" }),
    })).body);
    expect(updated.data.name).toBe("运营2");
    expect(updated.data.status).toBe(0);

    const deleted = JSON.parse((await client.del("/role-item", {
      headers: h, body: String(id),
    })).body);
    expect(deleted.code).toBe(0);

    const gone = JSON.parse((await client.del("/role-item", {
      headers: h, body: String(id),
    })).body);
    expect(gone.code).toBe(404);
  });

  it("GET /role-menu → 精简菜单树（根无 parentId）", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/role-menu", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.data.length).toBe(8);
    expect(body.data[0].id).toBe(100);
    expect(body.data[0].parentId).toBe(undefined);
    expect(body.data[1].parentId).toBe(100);
  });

  it("GET /menu-by-role-id?id=1 → 8 个 id；id=2 → []", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const all = JSON.parse((await client.get("/menu-by-role-id?id=1", { headers: h })).body);
    expect(all.data.length).toBe(8);
    expect(all.data[0]).toBe(100);
    const none = JSON.parse((await client.get("/menu-by-role-id?id=2", { headers: h })).body);
    expect(none.data.length).toBe(0);
  });
});

describe("admin menu", () => {
  it("GET /menu-list → 8 条种子菜单，keepAlive 为 boolean，order 可缺省", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/menu-list", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.total).toBe(8);
    const root = body.data.list[0];
    expect(root.id).toBe(100);
    expect(root.parentId).toBe("");
    expect(root.keepAlive).toBe(true);
    expect(root.order).toBe(100);
    const leaf = body.data.list[1];
    expect(leaf.parentId).toBe(100);
    expect(leaf.order).toBe(undefined);   // sort 为 NULL → order 缺省（对齐 mock）
  });

  it("menu-item POST→PUT→DELETE 闭环（DELETE body 为裸数字）", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const created = JSON.parse((await client.post("/menu-item", {
      headers: h,
      body: JSON.stringify({ parentId: 100, menuType: 0, name: "system:menu.audit", path: "/system/audit", component: "/system/audit", icon: "AuditOutlined", keepAlive: true, status: 1 }),
    })).body);
    expect(created.code).toBe(0);
    const id = created.data.id;
    expect(id > 0).toBeTruthy();

    const updated = JSON.parse((await client.put("/menu-item", {
      headers: h,
      body: JSON.stringify({ id, parentId: 100, menuType: 0, name: "system:menu.audit2", path: "/system/audit2", component: "/system/audit2", icon: "AuditOutlined", keepAlive: false, status: 1 }),
    })).body);
    expect(updated.data.name).toBe("system:menu.audit2");
    expect(updated.data.keepAlive).toBe(false);

    const deleted = JSON.parse((await client.del("/menu-item", {
      headers: h, body: String(id),
    })).body);
    expect(deleted.code).toBe(0);
  });
});
