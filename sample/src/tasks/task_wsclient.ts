export {};
// WS 客户端任务案例（v0.1.7）：连本机 WS 服务端 /v1/api/news/ws，首帧订阅 "news"，
// 循环收取 bus.publish("news", ...) 的广播帧。写法同 Kafka/RabbitMQ 消费任务
// （docs/mq-tasks.md），但重连不手写——断连即抛错 → Crashed → 监督器指数退避重启
// （1s→2s→…cap 60s）→ 新实例重连。
//
//   cargo run -p oj --release -- server -c sample/config.yaml --api-path sample/src
//   curl -X POST http://localhost:9778/v1/api/news -H 'X-TENANT-ID: t1' -d '{"text":"hi"}'
//   → 任务日志：ws frame {"text":"hi"}；Ctrl-C → stopped（Stopped 出口 ws.close()）
//
// 注意：wss:// 需宿主注入根证书（v0.1.7 未配，握手会失败）——见 api-manual「WebSocket」节。
const url = "ws://localhost:9778/v1/api/news/ws";

const frames: string[] = [];
let wake: (() => void) | null = null;
let closed: string | null = null; // 断连原因；非 null 时下一轮抛错 → Crashed → 监督重启

const ws = new WebSocket(url);
const opened = new Promise<void>((ok, err) => {
  ws.onopen = () => ok();
  ws.onerror = () => err(new Error("ws connect failed: " + url));
});
ws.onmessage = (e) => {
  frames.push(String(e.data));
  wake?.();
  wake = null;
};
ws.onclose = () => {
  closed = "ws closed";
  wake?.();
  wake = null;
};

await opened;
ws.send("{}"); // 首帧：服务端 ws.ts 执行 bus.subscribe("news")
log.info("ws task connected " + url);

while (!tasks.stopping()) {
  if (closed) throw new Error(closed); // 断连 → Crashed（超时/异常出口，非 Stopped）
  if (frames.length) {
    log.info("ws frame " + frames.shift());
    continue;
  }
  // 等帧/断连，与停机信号竞速：轮询间隔必须 ≪ stop_grace_secs（mq-tasks 纪律），
  // 否则停在 await 上等不到 wake，会被看门狗强杀记 killed 而非自然 stopped。
  await Promise.race([
    new Promise<void>((ok) => (wake = ok)),
    tasks.sleep(250),
  ]);
}
ws.close(); // Stopped 出口：干净断连
