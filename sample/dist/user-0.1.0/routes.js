// 由 oj build 生成；勿手改。
export default [
  { method: "get", pattern: "user/account", file: "account/api.js" },
  { method: "post", pattern: "user/account", file: "account/api.js" },
  { method: "put", pattern: "user/account", file: "account/api.js" },
  { method: "del", pattern: "user/account", file: "account/api.js" },
  { method: "patch", pattern: "user/account", file: "account/api.js" },
  { method: "head", pattern: "user/account", file: "account/api.js" },
  { method: "options", pattern: "user/account", file: "account/api.js" },
  { method: "get", pattern: "user/item/{id}", file: "item/api.js" },
  { method: "get", pattern: "user/profile", file: "profile/api.js" },
  { method: "post", pattern: "user/profile", file: "profile/api.js" },
  { method: "get", pattern: "user/profile/detail", file: "profile/detail/api.js" },
];
