// WS 帧池契约（目录镜像路由 /v1/api/news/ws）：
// export default { connection, message, close, error }——钩子语义与 v0.1.9 一致。
// v0.1.10 起执行模型为「帧池」：路由级 W 个无状态 Worker 共享执行，连接状态放
// sess.state（Rust 会话表持久，可 JSON 序列化）；模块作用域 = Worker 本地只读缓存。
export default {
  connection() {
    sess.state.ready = true; // 演示：会话状态外置（跨帧持久、按连接隔离）
    bus.subscribe("news");
    json.ok({ subscribed: true });
  },
};
