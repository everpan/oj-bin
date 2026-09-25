// 目录段 `_id_` 即路径参数（v0.1.27）：/v1/api/user/{id} 可达，
// 无需 .route 声明；避免 {} 进文件路径。文件系统中整段 `_name_` 映射 URL `{name}`。
export default {
  get() {
    json.ok({ id: http.param("id") });
  },
};
