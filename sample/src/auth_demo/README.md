# auth_demo — JWT 鉴权演示

配置（sample/config.yaml）`auth:` 块存在即启用；seed.sql 已建 `users` 表并写入
demo 用户（`demo` / `demo1234`，角色 admin）。

```bash
# 1. 登录换 token（匿名可达的是 /auth/* 内置路由）
curl -s -X POST http://localhost:9778/v1/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"demo","password":"demo1234"}'

# 2. 带 access_token 访问受保护路由
curl -s http://localhost:9778/v1/api/auth_demo/me/ \
  -H "Authorization: Bearer <access_token>"

# 3. 匿名路由（auth.anonymous_paths: ["/health"]）
curl -s http://localhost:9778/v1/api/auth_demo/health/

# 4. refresh 轮换（旧 refresh_token 立即失效）
curl -s -X POST http://localhost:9778/v1/api/auth/refresh \
  -H 'Content-Type: application/json' \
  -d '{"refresh_token":"<refresh_token>"}'

# 5. 登出（删 refresh session）
curl -s -X POST http://localhost:9778/v1/api/auth/logout \
  -H 'Content-Type: application/json' \
  -d '{"refresh_token":"<refresh_token>"}'
```

注意：本 sample 同时开了 tenant（X-TENANT-ID）——请求还需带 `-H 'X-TENANT-ID: acme'`。
