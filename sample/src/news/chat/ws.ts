// WS 帧内发布（目录镜像路由 /v1/api/news/chat/ws）：聊天室案例。
// 每帧整个文件重跑一遍：发 {"join":1} 进房（订阅 "chat"），
// 发 {"from":"neo","text":"hi"} 即广播给所有订阅连接（含本连接——Bus fan-out 不排除自己）。
// 帧代码是经典 script（非 ESM）：顶层不能 await；声明必须放进块作用域，
// 否则第二帧在同一 VM 重跑时 const 重复声明 → SyntaxError。
// 语义详解见 docs/websocket.md §2「帧内发布」。
bus.subscribe("chat");
{
  const frame = http.body;
  if (frame && frame.text) {
    bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
    json.ok({ sent: true });
  } else {
    json.ok({ joined: true });
  }
}
