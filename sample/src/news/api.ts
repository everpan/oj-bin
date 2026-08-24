// 发布路由（OJ-6 bus）：POST /v1/api/news（body {text}）→ 广播到所有订阅连接。
// WS 连接先连 /v1/api/news/ws 发任意一帧（WS.ts 订阅 news），再打本路由即可收到广播帧。
export default {
  async post(): Promise<void> {
    const b = http.body as { text?: string };
    await bus.publish("news", { text: b?.text ?? "hello" });
    json.ok({ published: true });
  },
};
