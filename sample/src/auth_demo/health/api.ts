// 匿名路由：config auth.anonymous_paths 含 "/health"，无需 token。
export default {
  get() {
    json.ok({ status: "ok", ts: Date.now() });
  },
};
