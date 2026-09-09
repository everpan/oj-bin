// WS 聊天室（目录镜像路由 /v1/api/news/chat/ws）。
// connection = 进房（订阅 "chat"，每连接一次）；message = 每帧广播
// {"from":"neo","text":"hi"} 给所有订阅连接（含自己——自回声）。
// 帧池模型：sess.state 存会话态（可序列化）；模块作用域只放只读缓存。
export default {
  connection() {
    sess.state.joined = true;
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
