// L2 纯 mock 单测：直接调用 sample/src/news/api.ts 的 handler 逻辑。
// 验证 POST /news 经 bus.publish("news", ...) 广播。

import { describe, it, expect } from "vitest";
import news from "../src/news/api";
import { invoke } from "./invoke";

describe("news (L2 mock)", () => {
  it("post publishes to bus with the given text", async () => {
    const r = await invoke(news, "post", { body: { text: "breaking" } });
    expect(r.code).toBe(0);
    expect(r.data.published).toBe(true);
    expect(r.published.length).toBe(1);
    expect(r.published[0].topic).toBe("news");
    expect(r.published[0].msg.text).toBe("breaking");
  });

  it("post falls back to default text when missing", async () => {
    const r = await invoke(news, "post", { body: {} });
    expect(r.code).toBe(0);
    expect(r.published[0].msg.text).toBe("hello");
  });
});
