// 由 oj build 生成的统一路由导出（此处为镜像路由的手工基线，oj build 上线后覆盖）。
// release 模式启动时一次性 import 本文件注册全部路由，免逐模块内省。
export default [
  { method: "get", pattern: "/v1/api/order/account", file: "order/account/api.js" },
  { method: "post", pattern: "/v1/api/order/account", file: "order/account/api.js" },
  { method: "get", pattern: "/v1/api/order/detail", file: "order/detail/api.js" },
  { method: "get", pattern: "/v1/api/order/list", file: "order/list/api.js" },
  { method: "get", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "post", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "put", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "patch", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "del", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "get", pattern: "/v1/api/user/profile", file: "user/profile/api.js" },
  { method: "post", pattern: "/v1/api/user/profile", file: "user/profile/api.js" },
  { method: "get", pattern: "/v1/api/user/profile/detail", file: "user/profile/detail/api.js" },
];
