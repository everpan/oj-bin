// L1 嵌套 handler 组集成测试（v0.1.64）：以实体对象为根聚合端点。
// 覆盖：目录镜像 / 实体组内动词与子资源 / 组级 .route 前缀 / 连字符实体名 /
// .schema 元数据隔离（契约 400 + 不生成 schema 路由）/ 方法缺席 405。
// 鉴权与租户头沿用 sample 既有约定（client.login + X-TENANT-ID）。

async function call(
  method: "get" | "post" | "put" | "del",
  path: string,
  body?: string,
): Promise<{ status: number; body: string }> {
  const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
  return client[method](path, {
    headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    body,
  });
}

describe("nest nested groups", () => {
  it("GET /nest/ → 模块根端点（目录镜像）", async () => {
    const r = await call("get", "/nest/");
    expect(r.status).toBe(200);
    expect(JSON.parse(r.body).data.where).toBe("get");
  });

  it("GET /nest/asset?state=ok → 实体组动词；缺 state → 400（契约在 JS 之前）", async () => {
    const bad = await call("get", "/nest/asset");
    expect(bad.status).toBe(400);
    const ok = await call("get", "/nest/asset?state=ok");
    expect(ok.status).toBe(200);
    expect(JSON.parse(ok.body).data.where).toBe("asset.get");
  });

  it("POST /nest/asset → 同实体另一动词", async () => {
    const r = await call("post", "/nest/asset", "{}");
    expect(r.status).toBe(200);
    expect(JSON.parse(r.body).data.where).toBe("asset.post");
  });

  it("POST /nest/asset/upload → 实体下的子资源（再嵌一层）", async () => {
    const r = await call("post", "/nest/asset/upload", "{}");
    expect(r.status).toBe(200);
    expect(JSON.parse(r.body).data.where).toBe("asset.upload.post");
  });

  it("POST /x/{anchor}/upload → 组级 .route 把整组挂到 base 根下", async () => {
    const r = await call("post", "/x/a1/upload", "{}");
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.data.where).toBe("anchor.post");
    expect(body.data.anchor).toBe("a1");
  });

  it("GET /nest/audit-log → 连字符实体名（计算式访问）", async () => {
    const r = await call("get", "/nest/audit-log");
    expect(r.status).toBe(200);
    expect(JSON.parse(r.body).data.where).toBe("audit-log.get");
  });

  it("元数据不生成路由：/nest/asset/schema → 404；只声明 POST 的路径 GET → 405", async () => {
    const none = await call("get", "/nest/asset/schema");
    expect(none.status).toBe(404);
    const m = await call("get", "/x/a1/upload"); // anchor 组只声明了 post
    expect(m.status).toBe(405);
  });
});
