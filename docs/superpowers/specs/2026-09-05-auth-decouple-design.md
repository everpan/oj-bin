# auth 解耦设计：原语入 bridge，端点 JS 化，守卫 cdylib 化

日期：2026-09-05
状态：已确认（方案 A）

## 背景与动机

当前 auth 内置在 `server` crate（`server/src/auth.rs` + `server/src/lib.rs` 的 `/auth/*`
内置路由 + Bearer 守卫）。解耦动机：让 auth 可定制（用户表结构、登录方式因项目而异）、
瘦 Rust 核心（核心不内置 users 表/bcrypt 等业务语义）、独立演进发布、示例即文档。

总原则：守卫是**安全边界**，留在机器码层（cdylib 插件）保证零开销与业务不可绕过；
端点是**业务逻辑**，JS 化获得定制自由。核心只剩密码学原语。

## 1. bridge 新增密码学原语

新增 `src/bridge/crypto.rs`，注册 op，bootstrap 挂全局对象：

```js
jwt.sign({ sub, roles })        // → access token；secret/alg/有效期来自注入配置，JS 摸不到密钥
jwt.verify(token)               // → claims 或抛错
jwt.refreshDuration             // → refresh 有效期秒数（注入配置，供 refresh 端点落 session 用）
bcrypt.hash(password)           // → hash
bcrypt.verify(password, hash)   // → bool
crypto.sha256Hex(s)             // → hex 摘要（session key 用）
crypto.randomHex(32)            // → 随机 hex 串（refresh token 用）
```

配置注入走现有 `Extras` 机制：新增 `Extras.jwt: Option<Arc<JwtCfg>>`
（secret / alg / access 有效期 / refresh 有效期），`oj/src/app.rs` 装配时从 `cfg.auth`
构建注入。未配置 `auth:` → `jwt.*` 报 "jwt not configured"（与 es 轴同款语义）。

## 2. 守卫 cdylib 化（FFI 第 6 轴）

- `server` crate 定义 trait：

```rust
pub trait AuthGuard: Send + Sync {
    /// Ok(None) = 匿名路径放行；Ok(Some(user)) = 注入 http.user；Err = 401。
    fn verify(&self, path_no_base: &str, authorization: Option<&str>)
        -> Result<Option<Value>, String>;
}
```

- `Pipeline.auth: Option<Arc<AuthGuard>>`；`handle()` 现有 Bearer 分支改为调这一个方法，
  匿名判断内收到插件实现里。
- `oj-plugin-ffi` 增 `AuthGuardVtable`（同步函数指针 + stabby 类型，无 async 跨边界）
  + `PluginRegistrations.auth` 槽位；**ABI_VERSION 5 → 6**（全部存量插件随之重编译）。
- 现有 `server/src/auth.rs` 的 `Auth` 搬到 `plugins/oj-auth`，实现该 vtable；
  `anonymous_paths` 经插件 cfg JSON 传入。
- `oj/src/app.rs` 装配：配了 `auth:` 就要求 auth 插件存在（缺失 fail-fast），vtable 包成
  `Arc<dyn AuthGuard>` 塞进 Pipeline。
- `handle()` 中 `login/refresh/logout` 三个内置路由分支**删除**——变为普通业务路由。
- `server` crate 只留 trait + pipeline 调用点。

## 3. JS auth 示例（与当前 Rust 逻辑逐行对齐）

`sample/src/auth/` 三个模块：

```
sample/src/auth/login/api.ts    → POST /v1/api/auth/login/
sample/src/auth/refresh/api.ts  → POST /v1/api/auth/refresh/
sample/src/auth/logout/api.ts   → POST /v1/api/auth/logout/
```

- **login**：`db.query` 查 users 表（username 走绑定参数）→ 用户不存在或
  `bcrypt.verify` 失败统一 `json.fail(401, "invalid credentials")`（不泄露用户存在性）→
  `jwt.sign({sub, roles})` + `crypto.randomHex(32)` 生成 refresh →
  `kv.set("AUTH-SESSION:" + crypto.sha256Hex(rt), {uid, exp})` →
  `json.ok({access_token, refresh_token, expires_in, user})`。
- **refresh**：查 session（惰性判 exp）→ 重查库取最新 roles → 删旧 session（一次一用）
  → 签新 token 对。
- **logout**：删 session。
- users 表结构（`id / username / password_hash(bcrypt) / roles(JSON 数组字符串)`）与
  roles 约定不变；模块自带 `schema.sql` / `seed.sql` 建表灌 demo 用户（顺带演示模块机制）。
- 用户表名不再走 `auth.user_table` 配置——写在 JS 里，业务自行修改。

## 4. 配置与兼容

| 配置项 | 去向 |
|---|---|
| `jwt_secret` / `signing_method` / `access_token_duration` | `Extras.jwt`，供 jwt op |
| `refresh_token_duration` | `Extras.jwt`，经 `jwt.refreshDuration` 暴露给 JS |
| `anonymous_paths` | 传给 oj-auth 插件 cfg JSON |
| `user_table` | **删除**（JS 示例写死 `users`，业务自改） |

路由路径：旧内置路由 `/v1/api/auth/login`（无尾斜杠）与新业务路由
`/v1/api/auth/login/` 经 `routes::normalize` 对齐，行为不变。

已知取舍（与现状一致，不在本期处理）：
- refresh 轮换 get→del 非原子（并发两次 refresh 各签一对；跨进程原子换发待 KV 原子 get+del）。
- logout 后 access token 到期前仍有效（JWT 无服务端吊销）。

## 测试

- `server` 的 `auth_full_pipeline` e2e 改为走 oj-auth 插件 + JS 端点全链路。
- `plugins/oj-auth` 自带单元测试：sign/verify/篡改/过期/匿名匹配/refresh 轮换/logout。
- bridge 层 jwt/bcrypt/crypto op 单测（含未配置报错路径）。
- JS 示例端点由 `oj/tests/e2e.rs` 或 `oj test` 覆盖 login→Bearer→refresh→logout 全流程。
