// L2：handler 分支 + 「发出了什么 SQL」。
//
// L1（`sample/tests/user.test.ts`）已覆盖 list / create / invalid role→400 / OPTIONS
// 四个**契约**用例，此处不再双写。本层补的是 L1 看不见的东西：
//   - 带了 id 走 id 过滤查询、不带 id 走全表查询（分支 + 绑定参数）；
//   - 缺 name → 400 "name required"，且**不落任何 SQL**（校验失败不能写库）；
//   - role 缺省回落 "user"；
//   - put / del / patch 的参数校验分支（L1 的 account 只打 get/post）。

import { describe, it, expect } from "vitest";
import account from "../src/user/account/api";
import { invoke } from "./invoke";
import { lastSqlCalls } from "./mocks/oj-globals";

describe("user/account (L2 分支 + SQL)", () => {
  it("get 无 id → 全表查询；有 id → 参数化 id 过滤", async () => {
    await invoke(account, "get", { dbRows: [] });
    const list = lastSqlCalls()[0];
    expect(list.fn).toBe("query");
    expect(list.sql).toBe("select id, name, role from account");
    expect(list.params).toEqual([]);

    await invoke(account, "get", { params: { id: "7" }, dbRows: [] });
    const one = lastSqlCalls()[0];
    expect(one.sql).toContain("where id = ?");
    expect(one.params).toEqual([7]);
  });

  it("post 缺 name → 400，且不落任何 SQL", async () => {
    const r = await invoke(account, "post", { body: { role: "admin" } });
    expect(r.code).toBe(400);
    expect(r.msg).toBe("name required");
    expect(lastSqlCalls()).toEqual([]);
  });

  it("post 缺省 role 回落 'user'", async () => {
    const r = await invoke(account, "post", { body: { name: "tank" } });
    expect(r.code).toBe(0);
    expect(lastSqlCalls()[0].params).toEqual(["tank", "user"]);
  });

  it("put 缺 id 或 name → 400，不落 SQL", async () => {
    expect((await invoke(account, "put", { body: { name: "x" } })).code).toBe(400);
    expect((await invoke(account, "put", { body: { id: 1 } })).code).toBe(400);
    expect(lastSqlCalls()).toEqual([]);
  });

  it("del/patch 非法 id → positiveId 守卫抛错（不落 SQL）", async () => {
    await expect(invoke(account, "del", { params: { id: "0" } })).rejects.toThrow();
    await expect(invoke(account, "patch", { body: { id: -1, role: "admin" } })).rejects.toThrow();
    expect(lastSqlCalls()).toEqual([]);
  });
});
