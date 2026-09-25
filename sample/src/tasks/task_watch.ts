// 池化长任务最小示例（v0.1.28）：导出 loop_body 即走任务池——worker 每轮调用一次，
// 无需 tasks.stopping() 轮询 / tasks.sleep()；停机时先跑 teardown 再退出。
// 单轮超时 tasks.pool.loop_body_timeout_ms（默认 5s）→ teardown + failed 退避重连，
// 长轮询（一次等数秒）任务不要用池化，继续用 TLA 写法（见 task_demo.ts）。
//
//   oj server -c sample/config.yaml --api-path sample/src
//   → GET  /v1/api/tasks                    任务清单（?type=long|cron）
//   → POST /v1/api/tasks/watch/stop         停止（触发 teardown）
//   → POST /v1/api/tasks/watch/reload       重连（重跑 setup）
//   → GET  /v1/api/tasks/watch/logs         执行事件（薄信封，内存环形缓冲）
//   （run-once 仅 cron 任务：见 jobs/report.ts）
// 轮间节奏 tasks.pool.interval_ms（默认 100ms）——每轮返回后框架 sleep 再调下一轮。
export async function setup() {
  log.info("watch connected");
}

export async function loop_body() {
  const n = Number((await kv.get("watch:last")) ?? "0");
  await kv.set("watch:last", String(n + 1));
  log.info(`watch tick #${n + 1}`);
}

export async function teardown() {
  log.info("watch down（尽力而为：超时不保证跑完）");
}
