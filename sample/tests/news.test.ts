// L1 新闻广播集成测试：POST /news 经 bus.publish 广播（进程内无 WS 订阅者）。

describe("news", () => {
  it("publish without auth → 401", async () => {
    const r = await client.post("/news", {
      headers: { "X-TENANT-ID": "default" },
      body: JSON.stringify({ text: "hi" }),
    });
    expect(r.status).toBe(401);
  });

  it("publish with auth + tenant → 200 published", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.post("/news", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
      body: JSON.stringify({ text: "breaking" }),
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.data.published).toBeTruthy();
  });
});
