// L1 WS 帧验收（v0.1.16）：client.ws 发 Uint8Array([1,2,3]) → 回显字节相等且帧型为
// binary；文本帧回显走 string（帧型由 send 参数类型决定）。
describe("echo-bin", () => {
  it("binary frame roundtrip is byte-exact", async () => {
    const ws = client.ws("/echo-bin/ws");
    await ws.send(new Uint8Array([1, 2, 3]));
    const frame = await ws.next();
    expect(frame.binary).toBe(true);
    expect(Array.from(frame.data)).toEqual([1, 2, 3]);
    // 非 UTF-8 字节也不得被 lossy 破坏
    await ws.send(new Uint8Array([0, 159, 255]));
    const f2 = await ws.next();
    expect(f2.binary).toBe(true);
    expect(Array.from(f2.data)).toEqual([0, 159, 255]);
    await ws.close();
  });

  it("text frame in -> text frame out (send picks frame type)", async () => {
    const ws = client.ws("/echo-bin/ws");
    await ws.send("ping");
    const frame = await ws.next();
    // 回显侧按入帧类型回帧：文本帧 http.body 非 null → ws.send(string) → Text 帧
    expect(frame.binary).toBe(false);
    expect(frame.data).toBe("ping");
    await ws.close();
  });

  it("text frame decode survives 2/3/4-byte UTF-8", async () => {
    const ws = client.ws("/echo-bin/ws");
    const s = "ping ✓ 汉字 😀"; // 2 字节 + 3 字节 + 4 字节（代理对）
    await ws.send(s);
    const frame = await ws.next();
    expect(frame.binary).toBe(false);
    expect(frame.data).toBe(s);
    await ws.close();
  });
});

// 回归（v0.1.16 补）：**服务端主动推帧**必须在客户端发首帧前可读。
// 原实现只在 send() 里惰性建连 → next() 拿 null 句柄调 op_client_ws_next，报
// `TypeError: expected u64`；而 connection 钩子里的 ws.send（本用例 = /news/ws 的
// json.ok 信封）与 bus 广播都不需要客户端先说话。
describe("ws server-push", () => {
  it("connection 钩子的帧在未 send 时可读（next 自己建连）", async () => {
    const ws = client.ws("/news/ws");
    const frame = await ws.next();
    expect(frame.binary).toBe(false);
    expect(JSON.parse(frame.data).data).toEqual({ subscribed: true });
    await ws.close();
  });
});
