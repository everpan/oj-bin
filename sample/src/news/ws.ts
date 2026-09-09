// WS 生命周期契约（目录镜像路由 /v1/api/news/ws）：
// export default { connection, message, close, error }——connection 在连接建立后
// 恰好触发一次（bus.subscribe 在此，订阅每连接一次，不再每帧重复）；
// message 每帧触发，帧内容经 http.body 读取（JSON 文本帧自动 parse）。
// 模块作用域即连接状态，跨帧存活；跨连接共享走 kv / bus。TS 语法可用（统一转译管线）。
export default {
  connection() {
    bus.subscribe("news");
    json.ok({ subscribed: true });
  },
};
