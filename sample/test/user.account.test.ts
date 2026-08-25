// L2 纯 mock 单测：直接调用 sample/src/user/account/api.ts 的 handler 逻辑，
// 用 mock 全局替代 db/json/http（不启动 server、不跑 v8）。

import { describe, it, expect } from "vitest";
import account from "../src/user/account/api";
import { invoke } from "./invoke";

describe("user/account (L2 mock)", () => {
  it("get lists accounts from dbRows", async () => {
    const r = await invoke(account, "get", {
      dbRows: [{ id: 1, name: "neo", role: "admin" }],
    });
    expect(r.code).toBe(0);
    expect(Array.isArray(r.data)).toBe(true);
    expect(r.data.length).toBe(1);
    expect(r.data[0].name).toBe("neo");
  });

  it("post creates an account", async () => {
    const r = await invoke(account, "post", {
      body: { name: "tank", role: "user" },
    });
    expect(r.code).toBe(0);
    expect(r.data.created).toBe(true);
  });

  it("post rejects invalid role → 400", async () => {
    const r = await invoke(account, "post", {
      body: { name: "x", role: "king" },
    });
    expect(r.code).toBe(400);
  });

  it("options reports supported methods", async () => {
    const r = await invoke(account, "options");
    expect(r.code).toBe(0);
    expect(r.data.methods).toContain("post");
  });
});
