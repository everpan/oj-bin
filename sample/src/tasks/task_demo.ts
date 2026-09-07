export {};
// 长任务最小示例（与 broker 无关）：每秒心跳，直到停机信号。
// 运行时无 timer 全局（setTimeout 不可用），等待一律用 await tasks.sleep(ms)。
//
//   oj server -c sample/config.yaml --api-path sample/src
//   → 启动日志：task: demo (task_demo.ts) → started
//   → Ctrl-C / SIGTERM → task: demo → stopped（grace 内自然收场）
//
// 消费型任务（Kafka/RabbitMQ）写法见 docs/devkit/api-manual.md「命名 MQ 客户端与长任务」：
//   const k = Kafka("default");
//   while (!tasks.stopping()) {
//     const { messages } = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
//     for (const m of messages) { /* 业务处理 */ }
//     if (messages.length) await k.commit(messages[messages.length - 1]);
//   }
while (!tasks.stopping()) {
  await tasks.sleep(1000);
  log.info("demo tick");
}
