// 最简受保护 GET：无 .route → 目录镜像路由 /v1/api/auth_demo/health/。
// 带有效 Bearer token 可达；真正的匿名端点是框架内置 /v1/api/health（证书状态），
// 由 config auth.anonymous_paths 的 "/health" 豁免。
export default {
  get() {
    json.ok({ status: "ok", ts: Date.now() });
  },
};
