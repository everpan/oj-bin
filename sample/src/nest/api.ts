// 嵌套 handler 组示例（v0.1.64）：以**实体对象为根**聚合端点——同一实体的多个
// 同动词端点聚在一个组里，不必为每个动词拆子目录，也不必把 api.ts 撑成大文件。
// 派生路由（base 缺省 /v1/api）：
//   GET  /v1/api/nest/                → get（模块根端点，目录镜像）
//   GET  /v1/api/nest/asset?state=ok  → asset.get（缺 state → 400，契约见 .schema）
//   POST /v1/api/nest/asset           → asset.post
//   POST /v1/api/nest/asset/upload    → asset.upload.post（实体下的子资源，再嵌一层）
//   POST /v1/api/x/{anchor}/upload    → anchor.post（组级 .route：整组挂 base 根下）
//   GET  /v1/api/nest/audit-log       → "audit-log".get（连字符实体名，计算式访问）
function get(): void {
  json.ok({ where: "get" });
}

// 实体为根：asset 的集合读/建/上传聚在一组（替代 asset-get/、asset-post/、
// asset-upload/ 三个目录；组名即路径段，与目录镜像同构）
const asset = {
  get: () => {
    json.ok({ where: "asset.get", state: http.query.state });
  },
  post: () => {
    json.ok({ where: "asset.post" });
  },
  // 子资源再嵌一层：无 .route → 父前缀 + 组名
  upload: {
    post: async () => {
      json.ok({ where: "asset.upload.post" });
    },
  },
};
// 元数据只挂 handler 函数：入参契约（JS 之前校验），不生成任何路由
asset.get.schema = {
  query: { type: "object", required: ["state"], properties: { state: { type: "string" } } },
};

// 对外根级 URL 的实体：组级 .route 作整组默认前缀（绝对 → base 根下）
const anchor = {
  post: async () => {
    json.ok({ where: "anchor.post", anchor: http.param("anchor", "") });
  },
};
anchor.route = "/x/{anchor}/upload";

// 实体名可以是非标识符（连字符）：段名禁 `/` 与原型三命名，其余计算式访问
const auditLog = {
  get: () => {
    json.ok({ where: "audit-log.get" });
  },
};

export default { get, asset, anchor, "audit-log": auditLog };
