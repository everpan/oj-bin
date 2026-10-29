// 防漂移门禁 · 腿①（REQ-02 / U-18 批 T2.2）：运行时全局的「存在性 × 全局可见性」golden。
// 本文件以业务模块形态（无 import）编译并运行——global.d.ts 把某全局声明进错的作用域
// （如曾发生在 audit/mgmt/secret 上的模块作用域事故）时，这里第一个红。
// 圈定口径：只断言**无条件挂载**的全局；条件挂载面（mail/es/cert/jwt/oidc/sso/blob/fs/
// exec/server/Kafka/RabbitMQ/redis 等，随配置缺席）不进清单（已知限制，见执行计划 §4）。

describe("runtime globals golden (unconditional surface)", () => {
  it("top-level globals exist and are visible module-free", () => {
    const fns = [
      "json",
      "http",
      "log",
      "kv",
      "db",
      "DB",
      "ws",
      "sess",
      "bus",
      "vars",
      "bcrypt",
      "fetch",
      "plugins",
      "axis",
      "ojInfo",
      "Kafka",
      "RabbitMQ",
      "tasks",
      "toBigInt",
      "toUBigInt",
      "toDouble",
      "audit",
      "mgmt",
      "secret",
    ];
    for (const n of fns) {
      const v = (globalThis as Record<string, unknown>)[n];
      expect(v !== undefined && v !== null).toBeTruthy();
    }
  });

  it("db sub-surfaces exist (access/epoch/client/session/cancel/catalog)", () => {
    expect(typeof db.access).toBe("object");
    expect(typeof db.access.grant).toBe("function");
    expect(typeof db.access.revoke).toBe("function");
    expect(typeof db.epoch).toBe("function");
    expect(typeof db.client).toBe("object");
    expect(typeof db.client.list).toBe("function");
    expect(typeof db.client.has).toBe("function");
    expect(typeof db.session).toBe("object");
    expect(typeof db.cancel).toBe("function");
    expect(typeof db.catalog).toBe("function");
  });

  it("secret surface shape (seal/open)", () => {
    expect(typeof secret.seal).toBe("function");
    expect(typeof secret.open).toBe("function");
  });

  it("db.client async methods return real Promises (REQ-06)", async () => {
    const p = db.client.list();
    expect(p instanceof Promise).toBeTruthy();
    expect(typeof p.catch).toBe("function");
    const n = await p.catch(() => []);
    expect(n.length).toBe(0);
  });
});
