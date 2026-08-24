// WS 帧循环（目录镜像路由 /v1/api/news/ws）：客户端每个文本帧执行一次本文件。
// 首帧订阅 "news" 主题：此后任意 handler 的 bus.publish("news", ...) 广播到本连接
// （含其它实例的 HTTP 发布）；帧内 json.ok 正常回信封。TS 语法可用（统一转译管线）。
bus.subscribe("news");
json.ok({ subscribed: true });
