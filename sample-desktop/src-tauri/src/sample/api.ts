// 目录镜像路由：src/sample/api.ts → /v1/api/sample/
// 由 Tauri 前端经 IPC → oj_dispatch → App::dispatch 在进程内调用（无端口）。
export default {
  get() {
    return json.ok({
      hello: "oj tauri",
      mode: "no-port",
      via: "App::dispatch",
      now: Date.now(),
    });
  },
};
