// http.module 演示（v0.1.76）：模块路由分发时框架注入的**只读**模块身份。
// - {name, version}：本请求命中的模块名与版本（dev 取 src/<m>/manifest.yaml，
//   release 取 dist/<m>-<v>/manifest.yaml）；
// - 无模块上下文（任务桥 / WS 帧 / run_with / 启动期内省）时为 null；
// - 对象已冻结：strict 下写属性抛 TypeError，CJS 下静默无效——框架授信面
//   （归属守卫 / bound_db / 平台特权换装）以 Rust 侧模块归属为据，不受 JS 影响。
// 典型用途：按模块版本做观测打点 / 兼容分支（版本分流本身在 manifests 锁与挂载层）。
export default {
  get() {
    json.ok({ module: http.module });
  },
};
