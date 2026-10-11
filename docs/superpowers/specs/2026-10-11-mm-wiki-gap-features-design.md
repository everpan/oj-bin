# 设计：mm-wiki 改写三缺口——HTTP/CAS SSO、引擎原生 FTS、内容版本化（v0.1.61）

日期：2026-10-11 · 状态：已评审（推荐组合定案）

> 来源：`mm-wiki/docs/rewrite/pr/` 三份提案（oj-http-sso / oj-search / oj-versioning），
> 经代码探查评估后修正落点。三份 PR 的共同前提「加轴 / 改插件 / 新全局」与代码现实不符，
> 本设计把落点移到**业务模块层**，宿主只做两处小补缺。

## 评审结论（对 PR 原案的关键修正）

| PR 前提 | 代码现实 | 修正 |
|---|---|---|
| 在 `oj-auth` 插件加 `auth.http_sso` 适配器 | oj-auth 是纯守卫（JWT/cookie 验签 + 匿名路径 + CSRF，514 行无 JS）；登录端点全是业务模块（`sample/src/auth/`、`sample/src/oidc/`） | 改插件 = 动 `AuthGuardVtable` = ABI bump + 全插件重编。改为 JS 模块 + 顶层 config 段（照 OidcCfg 模式），零 ABI 影响 |
| search 做成首方轴/全局，事务内同步索引 | `db.tx` 的 `resolve_target` 只对 db 族 op 生效；Rust 全局 op 须复刻 tx 路由才能进调用方事务 | search 本质是「方言 SQL 生成」，纯 JS 模块调 `db.tx` 天然同事务。MATCH 语法查询构造器（sea-query）本就不支持，须原生 SQL + 绑定参数（sql_guard 照常生效） |
| versions 的 `commit` 在 `db.tx` 内 `blob.put` | **blob 轴不参与事务**（`op_blob_put` 只查 `BlobRegistry`，不看 `ReqState.tx`） | 内容不进 blob 轴，进 db 表（sha256 主键 CAS 去重）——「内容+修订+head」同库同事务真原子。文本正文足够（PR 本就排除二进制） |

否决项（附理由）：宿主全局 search/versions（照 es 五件套，代码量 ~3-5×，且 tx 路由需复刻）；
泛型轴插件 `axis("search")`（对「只是 SQL」的东西纯属绕路：独立 cdylib + JSON 序列化 +
仍无事务联动）；blob 轴存版本内容（非原子）。

## 一、HTTP 表单 / CAS SSO（http-sso）

### 配置（config.yaml 顶层 `http_sso:` 段，列表）

```yaml
http_sso:
  - name: corp                     # 提供者名，唯一（装配期查重）
    kind: form                     # form | cas2
    login_url: "https://sso.corp.com/login"   # form: 凭据校验端点；cas2: CAS /login
    cas_validate_url: "https://sso.corp.com/serviceValidate"   # kind: cas2 必填
    user_field: username           # 缺省 username
    pass_field: password           # 缺省 password
    extra_fields: { app_id: mmwiki }
    success_when: { "data.code": 0 }   # 点路径 → 期望值，全部命中即成功；form 必填
    profile: { name: data.name, email: data.email }   # 点路径映射，可选
    jit: true                      # 缺省 true
```

装配期 fail-fast：name 重复 / kind 非法 / cas2 缺 `cas_validate_url` / form 缺 `success_when`。
`login_url` 非 https 仅 WARN（内网 http 联调常见，不拦）。

### Rust 改动（声明式，照 OidcCfg 模式）

- `src/config.rs`：`HttpSsoCfg`（`Vec<HttpSsoProvider>`）+ 上述装配期校验。
- `oj/src/app.rs`：注入 JS 全局 **`sso`**（`sso.providers` 数组；段不存在时调用报
  `http sso not configured`，与 `oidc not configured` 同形）。

### JS 模块（`sample/src/sso/`，拷走即用的参考实现）

- **form**：`POST /sso/login`——按 `user_field`/`pass_field`/`extra_fields` 组装 body
  POST `login_url` → `success_when` 点路径全命中即成功 → `profile` 点路径取用户信息 →
  JIT → `issueTokens`（与本地登录同构，场景 14 双 cookie）。失败统一
  `json.fail(401, "invalid credentials")`，**不回吐外部响应体**。
- **cas2**：`GET /sso/cas/login` 302 到 `{login_url}?service={callback}`；回跳
  `GET /sso/cas/callback?ticket=…` → fetch `serviceValidate` → 判
  `<cas:authenticationSuccess>`（正则提取，无需 XML 库）→ 取 `<cas:user>` → JIT →
  签发并 302 回前端。
- **JIT**：username 键 `sso:<provider>:<username>`（命名空间隔离，两不同信任级来源不得
  指同一键），`password_hash = "!sso"` 占位（非法 bcrypt 串，verify 恒 false，锁死密码
  登录，零 schema 变更）——与 OIDC 的 `oidc:<tenant>:<sub>` + `!oidc` 同形。
- `auth.anonymous_paths` 与 `tenant.anonymous_paths` **两条列表**都加
  `{ path: "/sso/cas/*", one_layer: true }`（302 腿带不了自定义头）；form 的
  `POST /sso/login` 是前端直调普通 API，不匿名。

### 裁剪（对 PR 的缩减）

- 砍 `success_expr`（表达式求值 = 注入面），只留声明式 `success_when`；极端格式业务
  handler 自写判定（它本来就是 JS）。
- 砍 `cas1`（`yes\n` 文本协议）；只做 `form` + `cas2`。
- `profile_expr` 降为点路径映射 `profile`。

## 二、引擎原生全文检索（search）

### Rust 改动（唯一一处）

`db.dialect()` op：返回 `"sqlite" | "mysql" | "postgres"`（读 `da.dialect()`，~10 行 +
bootstrap.js 一行 + `global.d.ts` 类型）。今天 JS 感知不到方言，这是 search 纯 JS 化的
唯一缺口。

### JS 共享模块（`sample/src/_shared/search.ts`）

```ts
await search.ensureIndex("docs", { fields: ["title", "body"], pk: "id" });
await search.index("docs", { id: 42, title: "部署手册", body: "…" });   // upsert by pk
await search.remove("docs", 42);
const hits = await search.query("docs", "部署", { limit: 20, highlight: true });
// hits = [{ id, score, fragments? }]
```

方言矩阵（`db.dialect()` 分派，全部走 `db.exec`/`db.query` + 绑定参数）：

| 方言 | 索引 | 查询 | 高亮 | 中文分词 |
|---|---|---|---|---|
| sqlite | FTS5 外部内容虚表 + 触发器同步（bundled 已开 `-DSQLITE_ENABLE_FTS5`） | `MATCH` | `snippet()` | `trigram`（3.46 内置） |
| mysql | `ALTER TABLE … ADD FULLTEXT … WITH PARSER ngram` | `MATCH…AGAINST(? IN NATURAL LANGUAGE MODE)` | 应用层（取回后 JS 标记） | `ngram`（服务端 ≥5.7.6） |
| postgres | `tsvector` 列 + GIN | `plainto_tsquery` | `ts_headline()` | `simple` 缺省；`zhparser` 有则配 |

- **score 不假装统一**：三引擎排序语义不同，返回引擎原始分 + 文档标注。
- **事务**：模块方法只发 SQL，调用方在 `db.tx(async () => { 写业务表; await search.index(…); })`
  内使用即天然同事务（这是选 JS 层的核心理由）。
- **安全**：标识符（表/列/索引名）一律 `valid_ident` 式校验 + 点位拼接；查询词只走绑定参数。

## 三、内容版本化（versions）

### 数据模型（两表，进 `sample/src/_platform/schema.yaml`，随模块拷走）

```yaml
oj_versions:        # 修订表：id, entity_type, entity_id, version, sha256, author, message, created_at
oj_ver_content:     # CAS 内容表：sha256 PK, content —— sha256 命中即 INSERT OR IGNORE 跳过（去重）
```

同库同事务完成「内容（若新）+ 修订行 + max(version) 推进」——真原子。

### JS 共享模块（`sample/src/_shared/versions.ts`）

```ts
const v = await versions.commit("doc", docId, content, { author, message, expect: 6 });
const body = await versions.get("doc", docId, 7);          // 7 | "head"
const log  = await versions.history("doc", docId, { limit: 20, offset: 0 });
const d    = await versions.diff("doc", docId, 6, 7);      // [{ type: "ctx"|"add"|"del", text }]
await versions.rollback("doc", docId, 6, { author, message: "回滚到 v6" });   // 重放为新版本
```

- **砍 head 指针表**：`max(version)`（索引列）即 head，少一张表。并发用**乐观锁**
  `expect: <version>`（与当前 max 不符即 fail `version conflict`）——CAS 语义比指针表实用。
- **diff**：JS 行级 LCS（~40 行 DP）+ `maxBytes` 上限（缺省 2MB，超限报错，文档标注）。
  不引 Rust `similar`（那是宿主全局方案才需要的）。
- `rollback` = 旧内容 commit 为新版本（不删历史，审计友好）；`purge` 文档标注仅超管
  （鉴权留给业务路由）。
- sha256 用既有 `crypto.sha256Hex`。
- id/序号用平台 schema 标准主键形态（跨引擎由 schema 层吸收）。

## 文档与发布（v0.1.61）

- `docs/devkit/` 四件同步：`api-manual.md`（`db.dialect()`、`sso` 全局段、错误/限制表）、
  `scenarios.md`（场景 28 已先行落 HTTPS/表单分层；再补 search / versions 场景）、
  `SKILL.md` 陷阱速查（中文 tokenizer、乐观锁撞版本、CAS 回跳 401——部分已落）、
  `README.md` 索引。场景 28（新增登录来源）已按本设计先行写入。
- CHANGELOG：feat ×3 + fix 归组；`oj/Cargo.toml` version → **0.1.61**；
  `cargo xtask build` 归置 `bin/devkit/`；发布点打标签（发版自查五步走 CHANGELOG 顶部清单）。
- 验收：mock SSO（sample 内 handler 伪装端点）登录 + JIT 建号 + 占位 hash 不可密码登录；
  cas2 mock `/serviceValidate` 解析；三引擎 search 增删改检索一致性（同事务）；versions
  commit×5 / get / diff / rollback / 乐观锁冲突 / 同内容二次 commit 不重复落内容表。
  e2e 进 `oj/tests/`（sqlite 腿必测；mysql/pg 腿视 CI 插件可用性）。

## 风险 / 已知边界

- 中文分词质量依引擎配置（ngram/zhparser/trigram），文档给矩阵不兜底召回率。
- mysql 高亮是应用层近似（无服务端 snippet 等价物）。
- `oj_ver_content` 存 db：文本正文场景无损；大二进制版本化将来另议（blob 不可变 key）。
- search/versions 若未来要「不拷模块的首方全局」，升级路径 = 现语义原样搬进
  `src/bridge/`（JS→Rust 直译），模块 API 不变——语义先验证、落点后升级。
