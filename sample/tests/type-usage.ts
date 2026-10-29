// 防漂移门禁 · 腿②（REQ-02 / U-18 批 T2.3）：tsc 类型用法测试。
// 按手册写法逐一调用全部声明面——CI 的 `tsc --noEmit` 编到本文件：声明缺失/签名漂移即红。
// 只编译不执行（文件名不以 .test.ts 结尾，oj test 不拾取）；签名必须与 global.d.ts 一致。

// db.access（U-1）：grant 至少一维；revoke 单参。
export async function accessUsage(): Promise<void> {
  await db.access.grant("rep", { roles: ["analyst"] });
  await db.access.grant("rep", { roles: ["analyst"], users: ["u1"], tenants: ["t1"], modules: ["dbx"] });
  await db.access.revoke("rep");
}

// db.epoch（U-3）：number 返回。
export function epochUsage(): number {
  return db.epoch("rep");
}

// json.stream / json.sse 的 fileName + disposition（v0.1.65）。
export function streamUsage(): void {
  json.stream({ contentType: "text/csv", fileName: "a.csv", disposition: "attachment" });
  json.stream({ status: 200, fileName: "b.jsonl" });
  json.sse({ heartbeatSecs: 15, fileName: "c.txt", disposition: "inline" });
}

// db.client 全方法（REQ-06 契约：异步方法 Promise、has 同步）。
export async function clientUsage(): Promise<void> {
  const entry = await db.client.register("rep", "sqlite://db.sqlite");
  const probed = await db.client.test("sqlite://db.sqlite");
  const again = await db.client.reRegister("rep", "sqlite://db.sqlite");
  const all = await db.client.list().catch(() => [entry]);
  const gone = await db.client.delete("rep").catch(() => false);
  const exists: boolean = db.client.has("rep");
  void probed;
  void again;
  void all;
  void gone;
  void exists;
}

// db 调用侧选项（REQ-05）：timeoutMs 收紧语句超时。
export async function callOptsUsage(): Promise<void> {
  await DB("rep").query("select 1", [], { timeoutMs: 5000 });
  await DB("rep").exec("delete from t where id = ?", [1], { timeoutMs: 5000 });
  // DBCallOpts 形状锚：只允许 timeoutMs 键。
  const opts: DBCallOpts = { timeoutMs: 100 };
  void opts;
}

/** U-18 任务式导出（v0.1.104）类型锚：按文档写法逐一调用。 */
export async function exportUsage(): Promise<void> {
  const { taskId } = await DB("rep").export({
    sql: "select * from orders where status = ?",
    params: ["paid"],
    format: "csv",
    fileName: "orders.csv",
    maxBytes: 64 * 1024 * 1024,
  });
  const st = await DB("rep").exportStatus(taskId);
  const state: "running" | "done" | "failed" | "cancelled" = st.state;
  const rows: number = st.rows;
  const bytes: number = st.bytes;
  const truncated: boolean = st.truncated;
  const reason: string | null = st.truncatedReason;
  const blobKey: string | null = st.blobKey;
  const error: string | null = st.error;
  if (state === "done" && blobKey) void [rows, bytes, truncated, reason, error];
  if (state === "failed") void error;
  const { cancelled } = await DB("rep").exportCancel(taskId);
  void cancelled;
}

// audit / mgmt / secret（特权检索与凭据面）。
export async function privilegedUsage(): Promise<void> {
  const events = await audit.query({ kind: "SECRET_OPEN", limit: 10 });
  void events;
  const connectors = await mgmt.list();
  void connectors;
  const alive: boolean = mgmt.has("x");
  void alive;
  const tok = secret.seal("plaintext");
  const plain = secret.open(tok);
  void plain;
}
