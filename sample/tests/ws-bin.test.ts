// L1 WS 二进制帧验收（v0.1.16）：client.ws 发 Uint8Array([1,2,3]) → 回显字节相等，
// 且帧型为 binary；文本帧回显仍走 string。
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

  it("text frame in -> bytes out via bodyBytes (send picks frame type)", async () => {
    const ws = client.ws("/echo-bin/ws");
    await ws.send("ping");
    const frame = await ws.next();
    // 回显侧 ws.send(Uint8Array) 固定发 binary 帧；字节仍与入帧一致
    expect(frame.binary).toBe(true);
    expect(Array.from(frame.data)).toEqual([112, 105, 110, 103]); // "ping"
    await ws.close();
  });
});
