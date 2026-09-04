# auth 模块

## 干什么

JWT 鉴权的三个业务端点：`POST /v1/api/auth/login`（bcrypt 校验 `_platform.users`，
签发 access + refresh 对）、`POST /v1/api/auth/refresh`（refresh 轮换，一次一用）、
`POST /v1/api/auth/logout`（删除 refresh session）。会话存 KV，键 =
`AUTH-SESSION:` + sha256(refresh_token)。Bearer 守卫（验签/匿名路径）由 oj-auth
插件在装配层提供，见 `sample/config.yaml` 的 `auth.anonymous_paths`——三个端点
须显式匿名。

## 怎么改

换登录方式（手机号/验证码/三方 OAuth）或换用户表：只改 `login/api.ts` 的取数与
校验段，签发逻辑收敛在 `_shared/session.ts` 的 `issueTokens`，不用动。
本模块通过 `deps: {_platform}` 读 `users` 表（`ownership_guard: deny` 下跨模块
读表必须声明）。

## 守卫

哪些路由要 Bearer、匿名路径怎么配，都在 oj-auth 插件 + `sample/config.yaml` 的
`auth` 段，不在本模块。
