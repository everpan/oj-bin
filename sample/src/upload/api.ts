// 文件上传 + blob（v0.2 OJ-5）：config.yaml blob: 段启用。
// POST /v1/api/upload/    multipart（file 字段）→ blob.put → 返回下载地址
// DEL  /v1/api/upload/?k={key}  删除 blob（幂等）
// GET  {base}/blob/{key}         公开下载路由（内置，不落业务表；local 直出 / s3 302 presign）
export default {
  async post() {
    const f = http.files[0];
    if (!f) json.fail(400, "need a file field (multipart)");
    const b = await http.file(0);
    await blob.put(f.filename, b, f.content_type);
    json.ok({ key: f.filename, url: await blob.url(f.filename), size: b.length });
  },
  async del() {
    await blob.del(http.param("k", ""));
    json.ok({ ok: true });
  },
};
