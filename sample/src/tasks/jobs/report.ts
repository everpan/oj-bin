// cron 脚本式任务示例（v0.1.28）：由 task/crontab.yaml 到点触发，整模块跑一次
// （顶层 await 即执行体），跑完释放 Worker。命名不用 task_ 前缀——否则会同时被
// 任务扫描器收编成长任务（同名拒启）。
//
// 手动立即跑一轮（鉴权/租户头与业务路由同语义）：
//   curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:9778/v1/api/tasks/report/run-once
export {};
const n = Number((await kv.get("report:runs")) ?? "0");
await kv.set("report:runs", String(n + 1));
log.info(`report ran #${n + 1}`);
