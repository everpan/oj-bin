// WS 聊天室（目录镜像路由 /v1/api/news/chat/ws）。
// 生命周期契约：connection = 进房（订阅 "chat"，每连接一次，无需 join 帧）；
// message = 每帧——发 {"from":"neo","text":"hi"} 即广播给所有订阅连接
// （含本连接——Bus fan-out 不排除自己）。
// 模块作用域即连接状态：不再是「每帧重跑整个文件」，顶层 const 可安全使用。
// 语义详解见 docs/websocket.md §2「帧内发布」。
export default {
  connection() {
    bus.subscribe("chat");
    json.ok({ joined: true });
  },
  message() {
    const frame = http.body;
    if (frame && frame.text) {
      bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
      json.ok({ sent: true });
    }
  },
};
