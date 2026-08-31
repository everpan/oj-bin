// L2 纯 mock 单测：admin 模块 handler 逻辑（不启动 server、不跑 v8）。

import { describe, it, expect } from "vitest";
import roleList from "../src/admin/role-list/api";
import { lineLength } from "../src/admin/home/line/api";
import { invoke } from "./invoke";

describe("admin/role-list (L2 mock)", () => {
  it("wraps dbRows into paged camelCase list", async () => {
    const r = await invoke(roleList, "get", {
      dbRows: [
        { id: 1, name: "超级管理员", code: "admin", status: 1, remark: "最高权限", create_time: 1729752330782, update_time: 1729752330782 },
      ],
    });
    expect(r.code).toBe(0);
    expect(r.data.total).toBe(1);
    expect(r.data.pageSize).toBe(10);
    expect(r.data.list[0].code).toBe("admin");
    expect(r.data.list[0].createTime).toBe(1729752330782);
  });

  it("respects pageSize/current params", async () => {
    const rows = [1, 2, 3].map((i) => ({ id: i, name: "r" + i, code: "c" + i, status: 1, remark: "", create_time: 1, update_time: 1 }));
    const r = await invoke(roleList, "get", { dbRows: rows, params: { pageSize: "2", current: "2" } });
    expect(r.data.total).toBe(3);
    expect(r.data.list.length).toBe(1);
    expect(r.data.list[0].id).toBe(3);
  });
});

describe("admin/home/line lineLength (L2)", () => {
  it("week → 7；未知 → 0", () => {
    expect(lineLength("week")).toBe(7);
    expect(lineLength("nope")).toBe(0);
  });

  it("month → 当天日号；year → 截至上月底累计天数", () => {
    const now = new Date();
    expect(lineLength("month")).toBe(now.getDate());
    let days = 0;
    for (let m = 0; m < now.getMonth(); m++) days += new Date(now.getFullYear(), m + 1, 0).getDate();
    expect(lineLength("year")).toBe(days);
  });
});
