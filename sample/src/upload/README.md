# upload — 文件上传 + blob 下载

配置（sample/config.yaml）`blob:` 段启用。local 驱动 root 相对 config_dir 绝对化。

```bash
# 1. 上传（multipart，file 字段；需带 tenant header——sample 开了 tenant）
curl -s -X POST http://localhost:9778/v1/api/upload/ \
  -H 'X-TENANT-ID: acme' \
  -F 'file=@a.png;type=image/png'

# 2. 下载（内置 {base}/blob/{key} 公开路由；local 直出 / s3 302 presign）
curl -s -o a.png http://localhost:9778/v1/api/blob/a.png

# 3. 删除（幂等）
curl -s -X DELETE 'http://localhost:9778/v1/api/upload/?k=a.png' \
  -H 'X-TENANT-ID: acme'

# 4. s3 驱动：config.yaml blob.driver="s3" + endpoint/bucket/region/access_key/secret_key/path_style
#    blob.url() 返回 presigned URL，浏览器/curl 直接 302 下载。
```

注意：上传路由走业务表（需 auth/tenant 守卫）；`{base}/blob/{key}` 下载路由公开、免鉴权。
