// 由 oj build 生成；勿手改（release 模式直载注册，见设计 §4.1）。
export default [
  { method: "get", pattern: "/v1/api/file/{*path}", file: "file/api.js" },
  { method: "get", pattern: "/v1/api/order/account", file: "order/account/api.js" },
  { method: "post", pattern: "/v1/api/order/account", file: "order/account/api.js" },
  { method: "get", pattern: "/v1/api/order/detail", file: "order/detail/api.js" },
  { method: "get", pattern: "/v1/api/order/list", file: "order/list/api.js" },
  { method: "get", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "post", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "put", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "del", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "patch", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "head", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "options", pattern: "/v1/api/user/account", file: "user/account/api.js" },
  { method: "get", pattern: "/v1/api/user/item/{id}", file: "user/item/api.js" },
  { method: "get", pattern: "/v1/api/user/profile", file: "user/profile/api.js" },
  { method: "post", pattern: "/v1/api/user/profile", file: "user/profile/api.js" },
  { method: "get", pattern: "/v1/api/user/profile/detail", file: "user/profile/detail/api.js" },
];
