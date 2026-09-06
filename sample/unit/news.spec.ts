// L2：bus 事件内容 + 默认文案分支。
//
// L1（`sample/tests/news.test.ts`）只断言「未鉴权 401」与「已鉴权 200 + published」——
// 它无法观察广播帧本身（进程内无 WS 订阅者）。「广播出去的帧长什么样」与
// 「缺省 text 回落 hello」这两个分支属于本层独有价值，故保留。

import { describe, it, expect } from "vitest";
import news from "../src/news/api";
import { invoke } from "./invoke";

describe("news (L2 bus 事件内容)", () => {
  it("publish 帧 = { topic: 'news', msg: { text } }", async () => {
    const r = await invoke(news, "post", { body: { text: "breaking" } });
    expect(r.code).toBe(0);
    expect(r.data.published).toBe(true);
    expect(r.published).toEqual([{ topic: "news", msg: { text: "breaking" } }]);
  });

  it("缺省 text 回落 'hello'（L1 覆盖不到的分支）", async () => {
    const r = await invoke(news, "post", { body: {} });
    expect(r.code).toBe(0);
    expect(r.published[0].msg.text).toBe("hello");
  });

  it("body 为 null 时也回落 'hello'，不抛错", async () => {
    const r = await invoke(news, "post", {});
    expect(r.code).toBe(0);
    expect(r.published[0].msg.text).toBe("hello");
  });
});
