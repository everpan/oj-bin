// L2 调用助手：装好 mock 全局后直接调用真实 handler 的某方法，flush 微任务后
// 返回本次响应的捕获（code/msg/data）+ 期间 bus 发布的事件。
// 注意：handler 内部用 db.query(...).then(cb) 走微任务，故需 setTimeout(0) flush。

import { installGlobals, lastPublished, ResponseCapture, GlobalsOptions } from "./mocks/oj-globals";

export interface InvokeResult extends ResponseCapture {
  published: Array<{ topic: string; msg: any }>;
}

export async function invoke(
  handler: Record<string, (...a: any[]) => any>,
  method: string,
  opts: GlobalsOptions = {},
): Promise<InvokeResult> {
  const cap = installGlobals(opts);
  (globalThis as any).http.method = method;
  await handler[method]?.();
  // 等待 handler 内 db.query(...).then(json.ok) 的微任务落地。
  await new Promise((r) => setTimeout(r, 0));
  return { ...cap, published: lastPublished() };
}
