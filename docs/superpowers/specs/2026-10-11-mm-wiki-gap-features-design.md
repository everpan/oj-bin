# 设计：mm-wiki 改写三缺口——HTTP/CAS SSO、引擎原生 FTS、内容版本化（v0.1.61）

日期：2026-10-11 · 状态：已评审（双专家：修改后通过，意见已并入）

> 来源：`mm-wiki/docs/rewrite/pr/` 三份提案（oj-http-sso / oj-search / oj-versioning），
> 经代码探查评估后修正落点。三份 PR 的共同前提「加轴 / 改插件 / 新全局」与代码现实不符，
> 本设计把落点移到**业务模块层**，宿主只做两处小补缺。

## 评审记录（2026-10-11，双专家独立评审，结论一致：修改后通过）

**架构师** H1（form 登录端点「不匿名」在守卫模型下 401，功能不可用——已采纳，并回改场景
28）、H2（cas2 回跳交接腿未设计：service 可控 = ticket 校验被引到攻击者侧、token 进
query 泄露、302 目标 open-redirect——已采纳，新增「cas2 回跳交接」节）、M1（JS 全局注入
机制账目失真：`app.rs 注入`不存在，实为 StableState/Extras 字段 + op + bootstrap.js 装配
约 5 触点——已采纳）、M2（HTTPS WARN-only 与场景 28「强制 HTTPS」矛盾，违背 SMTP
`allow_none_tls` 的显式 opt-in 先例——已采纳：缺省拒绝 + `allow_http: true`）、M3（凭据
政策缺位——已采纳，补 ENC 自动解封/暴露面声明/禁日志三条）、M4（JIT 键缺租户段，
users 表跨租户共享会混同——已采纳：键加 `<tenant>` 段）、M8（sql_guard 与 FTS 裸 SQL 的
边界未提——已采纳，DDL 走迁移 + deny 模式边界进限制表）、M11（拷走即用缺 deps/migrations
要件，S003 会 build 失败——已采纳）、L1（provider 未 pin 已配置集合 + name 禁 `:`）、
L3（list 改 map 形态免查重）、L5（trigram 措辞、升级路径点名 tx 路由复刻、dialect 口径）
——均采纳。

**工程师**（含本机 sqlite3 实测）S1（=H1）、S2（trigram 对 <3 字符查询静默零命中，
示例 `"部署"` 在 sqlite 返回空——已采纳：LIKE 回退 + 限制表）、S3（外部内容 FTS 表不暴露
业务主键列，只认 `rowid`，且要求业务 PK 为 INTEGER PRIMARY KEY——已采纳）、S4（`sha256
text PRIMARY KEY` 在 MySQL 报 1171 建不出表；schema.yaml 无 char 型、无唯一约束——已采纳：
改手写迁移，方言标签 `{sqlite,mysql,pg}`）、S5（乐观锁缺 UNIQUE 兜底，并发产生重复版本号；
重复键错误须映射 `409 version conflict`——已采纳）、S6（2MB 下 LCS DP 是 10^9 格，必爆
256MiB isolate 堆——已采纳：格子预算 4M 闸 + trim 公共前后缀）、S7（`sample/src/_shared/`
裸目录启动即报 `missing manifest.yaml`——已采纳：共享代码进 `_platform/_shared/`）、
S8（ensureIndex 的 DDL 在 mysql 会隐式提交调用方事务，db.rs:722 自证——已采纳：禁止
tx 内调用）、M9（success_when 判等语义未定 + mm-wiki 真实判据是 `message == ""`——已
采纳：字符串化宽松等价 + null 期望形态）、M10（出站编码未定，mm-wiki 用
x-www-form-urlencoded——已采纳：`body_encoding: form|json` 缺省 form；502/401 两态拆分）、
M11（CAS 正则三坑 + service 两腿必须同串——已采纳，正则入 spec）、M12（sqlx Any 不转换
占位符：pg 须 `$1`；pg 目录表标识符列须 `::text`；DDL 逐条 exec——已采纳）、M13（三引擎
幂等策略分别写清——已采纳）、M14（index() 语义 = 业务表 upsert，绝不直写 FTS 对象，
否则双写——已采纳）、M15（CAS 表无 tenant_id 与 sql_guard/tenant 缺省 true 的冲突——
已采纳：表不进 schema.yaml，走迁移，未登记表裸 SQL 放行同 `_oj_sequences`）、M16（mysql
InnoDB FULLTEXT 同事务可见性属文档含糊区——已采纳：验收标注「需实测」）、M17（sso e2e
须独立测试目标防 oj-auth OnceLock 污染；CI 现无 mysql/pg 真库，env-gated DSN 为新增基建
而非「视可用性」——已采纳）、L18/L19/L21——均采纳。

## 评审结论（对 PR 原案的关键修正）

| PR 前提 | 代码现实 | 修正 |
|---|---|---|
| 在 `oj-auth` 插件加 `auth.http_sso` 适配器 | oj-auth 是纯守卫（JWT/cookie 验签 + 匿名路径 + CSRF，514 行无 JS）；登录端点全是业务模块（`sample/src/auth/`、`sample/src/oidc/`） | 改插件 = 动 `AuthGuardVtable` = ABI bump + 全插件重编。改为 JS 模块 + 顶层 config 段（照 OidcCfg 模式），零 ABI 影响 |
| search 做成首方轴/全局，事务内同步索引 | `db.tx` 的 `resolve_target` 只对 db 族 op 生效；Rust 全局 op 须复刻 tx 路由才能进调用方事务（先例：`op_db_next_seq` db.rs:708 搭车 `Target::Tx`） | search 本质是「方言 SQL 生成」，纯 JS 模块调 `db.tx` 天然同事务。MATCH 语法查询构造器（sea-query）本就不支持，须原生 SQL + 绑定参数 |
| versions 的 `commit` 在 `db.tx` 内 `blob.put` | **blob 轴不参与事务**（`op_blob_put` 只查 `BlobRegistry`，不看 `ReqState.tx`） | 内容不进 blob 轴，进 db 表（CAS 去重）——「内容+修订」同库同事务真原子。文本正文足够（PR 本就排除二进制） |

否决项（附理由）：宿主全局 search/versions（照 es 五件套，代码量 ~3-5×，且 tx 路由需复刻）；
泛型轴插件 `axis("search")`（独立 cdylib + JSON 序列化 + 仍无事务联动）；blob 轴存版本
内容（非原子）。

---

## 一、HTTP 表单 / CAS SSO（http-sso）

### 配置（config.yaml 顶层 `http_sso:` 段，**map 形态**，键即提供者名，免查重）

```yaml
http_sso:
  corp:                            # 提供者名（禁含 `:`，JIT 键按 `:` 切分）
    kind: form                     # form | cas2
    login_url: "https://sso.corp.com/login"   # form: 凭据校验端点；cas2: CAS /login
    cas_validate_url: "https://sso.corp.com/serviceValidate"   # kind: cas2 必填
    user_field: username           # 缺省 username
    pass_field: password           # 缺省 password
    body_encoding: form            # form | json，缺省 form（mm-wiki 上游即 urlencoded）
    extra_fields: { app_id: mmwiki }
    success_when: { "message": "" }    # 点路径 → 期望值，全部命中即成功；form 必填
    profile: { name: data.name, email: data.email }   # 点路径映射，可选
    allow_http: false              # 缺省 false：login_url 非 https 装配期报错；显式 true 才放行（WARN）
    callback_url: "https://wiki.corp.com/v1/api/sso/cas/callback"   # cas2 必填（见「回跳交接」）
```

装配期 fail-fast（`src/config.rs` 新增 `validate_http_sso`，照 `validate_anon_paths` 形态，
挂 `oj/src/app.rs` 现有 validate 串）：kind 非法 / cas2 缺 `cas_validate_url`·`callback_url` /
form 缺 `success_when` / name 含 `:` / 非 https 且未 `allow_http`。
`success_when` 判等语义（进 api-manual）：点路径只支持 `.` 分隔对象遍历（拒 `__proto__`/
`constructor`，不支持数组下标）；**缺失或 null 仅当期望值为 `null` 时命中**；其余
`String(actual) === String(expected)`（`0` 同时命中数字 0 与 `"0"`）。mm-wiki 真实判据是
`message == ""`：`success_when: { message: "" }`（恒回字段时）或 `{ message: null }`
（缺省也算成功）。

### Rust 改动（约 5 触点，全部声明式）

1. `src/config.rs`：`HttpSsoCfg`（map）+ `validate_http_sso`。
2. `oj/src/app.rs`：装配构建（照 `build_jwt_and_oidc` 三件套）→ `Extras`/`StableState`
   加 `sso` 字段（照 `oidc` 槽 mod.rs:188/240）。
3. `src/bridge/` 新 op：返回 providers（段不存在报 `http sso not configured`，照
   bridge/oidc.rs:164）。
4. `src/bridge/bootstrap.js`：`globalThis.sso` 装配行（7-bit ASCII 红线照守）。
5. `sample/global.d.ts`：`sso` 类型。

### JS 模块（`sample/src/sso/`，普通业务模块：manifest + api.ts）

- **form**：`POST /sso/login`——按 `user_field`/`pass_field`/`extra_fields` 组装 body
  （`body_encoding: form` 用 `URLSearchParams` + `application/x-www-form-urlencoded`）POST
  `login_url` → `success_when` 全命中即成功 → `profile` 点路径取用户信息 → JIT →
  `issueTokens`（场景 14 双 cookie）。**两态拆分**：网络错/非 2xx/非 JSON →
  `json.fail(502, "sso upstream failed")`；判定失败 → `json.fail(401, "invalid
  credentials")`。**不回吐外部响应体**。
- **cas2**：`GET /sso/cas/login` 302 到 `{login_url}?service={callback_url}`（service 与
  校验腿**必须同串**，构造收口成一个函数两腿复用）；回跳
  `GET /sso/cas/callback?ticket=…` → fetch `cas_validate_url?ticket=…&service=…` → 判成功
  → 取 user → JIT → 签发 → 302 回前端。XML 判定：
  ```js
  const ok = /<[\w.-]*:?authenticationSuccess[\s/>]/.test(xml);   // 前缀可缺省，跨行安全
  const u = xml.match(/<(?:[\w.-]+:)?user(?:\s[^>]*)?>([\s\S]*?)<\/(?:[\w.-]+:)?user>/);
  ```
- **JIT**：username 键 **`sso:<provider>:<tenant>:<username>`**（tenant 取 `http.tenantId`，
  未启用租户为空串；users 表无租户列、跨租户共享，缺 tenant 段会混同——OIDC 键同教训）。
  `password_hash = "!sso"` 占位（非法 bcrypt 串，verify 恒 false，锁死密码登录）。
  **provider 必须命中 `http_sso` 已配置名，未命中 400**（防客户端自报高信任命名空间）。
- **匿名路径（评审修正：form 登录也必须匿名）**：守卫对非匿名路径无 token 一律 401，
  登录端点恰是无 token 时调用的（`/auth/login` 先例）。`auth.anonymous_paths` 加
  `/sso/login` 与 `/sso/cas/*`；`tenant.anonymous_paths` 同加两条（302 腿带不了自定义头/
  租户头；直调腿由前端带租户头，但加列表无害且与 OIDC 同口径）。
- **cas2 回跳交接（评审新增，安全敏感）**：`service`/回跳基准 = **装配期配置**
  （`callback_url`），禁从请求 Host/query 派生（防 ticket 校验被引到攻击者侧）；token
  **不进 URL**，走 httpOnly 双 cookie 交接；登录后 302 目标只接受「配置固定值」或
  「`/` 开头且非 `//` 开头的相对路径」，绝对 URL 一律 400（防 open-redirect）。
- **凭据**：段内敏感值（`extra_fields` 里的 secret 等）写 `ENC[...]`（v0.1.33 密封机制
  对全树自动解封，无私钥 fail-fast）；`sso.providers` 对 handler 全量可读，暴露面与
  `oidc.info`（含 client_secret）同级，文档声明；入站/出站凭据**禁入日志**。

### 裁剪（对 PR 的缩减）

砍 `success_expr`（表达式 = 注入面，声明式 `success_when` 覆盖 mm-wiki 场景）；砍 `cas1`
（只做 `form` + `cas2`）；`profile_expr` 降为点路径映射。

---

## 二、引擎原生全文检索（search）

### Rust 改动（唯一一处）：`db.dialect()`

```rust
// src/bridge/db.rs，紧邻 op_db_next_seq；同步 op，dialect 与事务无关，不需 resolve_target
#[op2] #[string]
pub fn op_db_dialect(state: Rc<RefCell<OpState>>, #[string] name: String) -> Result<String, JsErrorBox>
// Dialect::MySql => "mysql" | Dialect::Postgres => "postgres" | Dialect::Sqlite => "sqlite"
```

注册进 mod.rs ops 列表；bootstrap.js 在 **db 实例与 tx 回调对象**各加
`dialect: () => op_db_dialect(name)`（search 用全局 `db`，op 经 `resolve_target` 自动路由
进同名活跃 tx）；`sample/global.d.ts` 补 `dialect(): "sqlite" | "mysql" | "postgres"`。
三 accessor 全覆盖（SqlxAccessor / FfiDataAccessor 均有 `dialect()`）。
**口径（进 api-manual）**：`db.dialect()` 供 `_shared` 模块与诊断用——业务 handler 分支
方言 = 放弃「业务无需手写方言差异」承诺，慎用。

### JS 共享模块（`sample/src/_platform/_shared/search.ts`）

> 评审修正：`sample/src/_shared/` 裸目录会被模块发现报 `missing manifest.yaml`——共享代码
> 必须落在 `_platform` 模块内，引用方 `import { … } from "#_platform/_shared/search"` +
> `deps: { _platform: "^…" }`（与 versions 同家）。

```ts
await search.ensureIndex("docs", { fields: ["title", "body"], pk: "id" });
await search.index("docs", { id: 42, title: "部署手册", body: "…" });   // = 业务表 upsert
await search.remove("docs", 42);                                        // = 业务表 delete
const hits = await search.query("docs", "部署", { limit: 20, highlight: true });
```

**`index()`/`remove()` 语义 = 对业务表的 upsert/delete**（sqlite `INSERT OR REPLACE` /
mysql `ON DUPLICATE KEY UPDATE` / pg `ON CONFLICT (id) DO UPDATE`），**任何分支都不直写
FTS/tsvector 对象**——三引擎索引均随业务行自动维护，直写即双份。

方言矩阵（含评审补齐的幂等与占位符）：

| 方言 | 索引（幂等） | 查询 | 高亮 | 中文 | 注意 |
|---|---|---|---|---|---|
| sqlite | `CREATE VIRTUAL TABLE IF NOT EXISTS … USING fts5(…, content='docs', content_rowid='id')` + 触发器三件套（`IF NOT EXISTS`，AFTER I/D/U，delete 侧 `'delete'` 特写） | `SELECT rowid AS id, bm25(docs_fts) AS score … MATCH ? ORDER BY rank` | `snippet()` | `tokenize='trigram'` | **外部内容模式只认 rowid、要求业务 PK 为 INTEGER PRIMARY KEY**；查询词 **<3 字符 trigram 静默零命中 → 回退业务表 `LIKE ? ESCAPE '\'`（JS 转义 %/_/\，score=1）** |
| mysql | 先查 `information_schema.statistics` 再 `ALTER TABLE docs ADD FULLTEXT INDEX ft_docs (title, body) WITH PARSER ngram`（**无 IF NOT EXISTS**；ngram 需 5.7.6+） | `MATCH(…) AGAINST(? IN NATURAL LANGUAGE MODE)` 可同列 `AS score` | 应用层：`body.indexOf(query)` 截窗口（分词在索引侧，与原文无关） | `ngram` | 占位符 `?` |
| postgres | `ALTER TABLE … ADD COLUMN IF NOT EXISTS tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(title,'') || ' ' || coalesce(body,''))) STORED` + `CREATE INDEX IF NOT EXISTS … USING gin (tsv)`（PG12+；两参 to_tsvector 才 IMMUTABLE；不走触发器） | `tsv @@ plainto_tsquery('simple', $1)` | `ts_headline('simple', …)` | `simple` 缺省（中文按字）；有 `zhparser` 则配 | **占位符 `$1`（sqlx Any 不转换）**；目录表查询标识符列须 `::text` |

通用规则：占位符按方言渲染（`postgres ? $n : '?'`）；标识符（表/列/索引名）JS 侧
`/^[A-Za-z_][A-Za-z0-9_]*$/` 校验后拼接，查询词只走绑定参数；DDL 逐条 `db.exec`
（`TxSession::exec` 单语句，sqlite 触发器体含 `BEGIN…END;` 实测一条可过）。

**`ensureIndex` 禁止在 `db.tx` 内调用**：mysql DDL 隐式提交调用方事务（db.rs:722 自证
教训），PG 失败语句毒化事务（25P02）。它是池路径装配操作，模块内进程级 `ensured` Set
去重。**index/remove/query 在 tx 内调用即同事务**（这是选 JS 层的核心理由）。

**sql_guard 边界（进 api-manual 限制表）**：FTS 对象全部经模块迁移建（启动路径不经 op
守卫）；运行时 MATCH 裸 SQL 在启用租户守卫的库须带业务表 `tenant_id` 条件；
`sql_guard: deny` 模式与引擎原生 FTS 查询互斥（deny 部署要么关守卫要么不用 search）。
`docs_fts` 影子表会进 `oj schema diff` 的 D002 噪音（虚表计入 sqlite_master）——文档标注。

---

## 三、内容版本化（versions）

### 数据模型（手写迁移，**不进 schema.yaml**）

评审修正：`sha256 text PRIMARY KEY` 在 MySQL 报 1171（TEXT 不可作 PK），schema.yaml 又无
char 型、无唯一约束——两表改走
**`sample/src/_platform/migrations/0002__versions.{sqlite,mysql,pg}.sql`**（方言标签是
`pg`；schema.yaml 不声明，未登记表裸 SQL 放行，同 `_oj_sequences` 先例）：

- `oj_versions(id, entity_type, entity_id, version, sha256, author, message, created_at)`
  + **`UNIQUE(entity_type, entity_id, version)`**（乐观锁的兜底，schema.yaml 表达不了）。
  mysql：id `BIGINT AUTO_INCREMENT PRIMARY KEY`；pg：`BIGSERIAL`。
- `oj_ver_content(sha256 VARCHAR(64) PRIMARY KEY, content TEXT)`——CAS 去重。
- 消费模块声明 `deps: { _platform: "^…" }`（S003）。

### JS 共享模块（`sample/src/_platform/_shared/versions.ts`）

```ts
const v  = await versions.commit("doc", docId, content, { author, message, expect: 6 });
const body = await versions.get("doc", docId, 7);          // 7 | "head"（= max(version)）
const log  = await versions.history("doc", docId, { limit: 20, offset: 0 });
const d    = await versions.diff("doc", docId, 6, 7);      // [{ type: "ctx"|"add"|"del", text }]
await versions.rollback("doc", docId, 6, { author, message: "回滚到 v6" });   // 重放为新版本
```

- **乐观锁判定序（评审修正）**：`expect` 与当前 `max(version)` 不符 → fail(409)；相符则
  INSERT 修订行——并发双 commit 时**后插者**在 INSERT 处撞 UNIQUE（阻塞至对方 commit 后
  收重复键错，非 commit 时）→ JS 侧按 `/unique|duplicate/i` 匹配错误文案（三方言措辞
  不同）统一映射 `fail(409, "version conflict")`，**不做二次 max 复核**。不带 `expect` =
  last-write-wins（UNIQUE 仍保证版本号不重）。
- **CAS 去重**：`INSERT OR IGNORE`（sqlite）/ `INSERT IGNORE`（mysql）/
  `ON CONFLICT DO NOTHING`（pg）按方言分派——运行时 `db.exec` 无 seed 层的自动改写。
  sha256 用既有 `crypto.sha256Hex`。
- **diff（评审修正：格子预算闸，非字节闸）**：先 trim 公共前/后缀；修剪后任一侧行数使
  `a.length * b.length > 4_000_000`（≈16MB DP 矩阵）→ 报 `diff too large`（更大文档走
  Myers O(ND) 或降级整删整加，后续再议）；`maxBytes`（缺省 2MB）保留为二级闸，防把超大
  串拉进 JS。
- `rollback` = 旧内容 commit 为新版本（不删历史，审计友好）；`purge` 文档标注仅超管
  （鉴权留给业务路由）。

---

## 四、文档与发布（v0.1.61）

- `docs/devkit/` 四件同步：`api-manual.md`（`db.dialect()` 及口径、`sso` 全局段与
  success_when 语义、**错误/限制表新增**：trigram <3 字符、sqlite 腿整型 PK、
  `sql_guard: deny` 与 FTS 互斥、CAS 回跳红线（token 不进 URL/302 目标白名单）、
  mysql 同事务 FULLTEXT 可见性需实测、provider 须命中配置）；`scenarios.md`（**修订
  场景 28**：form 登录须匿名、HTTPS 缺省拒绝 `allow_http`、CAS 交接红线；再补 search /
  versions 场景）；`SKILL.md` 陷阱速查同步；`README.md` 索引。
- CHANGELOG：feat ×3 + fix 归组；`oj/Cargo.toml` version → **0.1.61**；
  `cargo xtask build` 归置 `bin/devkit/`；发布点打标签（CHANGELOG 顶部五步清单）。
- **测试（评审修正）**：sso e2e 为**独立测试目标**（oj-auth 插件 `OnceLock` 首次 init
  钉死 anonymous_paths，同进程会互相污染，oidc_e2e.rs 先例）；mock SSO 照
  oidc_e2e.rs 的 tmp_project 夹具模式。三引擎腿：CI 现无 mysql/pg 真库（无
  `services:` 容器先例）——新增 env-gated DSN（`OJ_TEST_MYSQL_DSN` / `OJ_TEST_PG_DSN`，
  缺省 skip）或 CI services 容器，属**新增基建**。sqlite 腿（FTS5）必测。
- 验收：mock SSO 登录 + JIT 建号（键含 tenant 段）+ 占位 hash 不可密码登录 + provider
  未配置 400 + http:// 缺省拒；cas2 mock `/serviceValidate` 解析 + 302 交接红线；
  search 增删改检索一致性（同事务）+ sqlite 短词 LIKE 回退 + `ensureIndex` 拒 tx 内；
  versions commit×5 / get / diff / rollback / `expect` 撞 UNIQUE → 409 / 同内容二次
  commit 不重复落内容表；mysql 腿同事务 MATCH 可见性**标注需实测**。

## 风险 / 已知边界

- 中文分词质量依引擎配置（ngram/zhparser/trigram），文档给矩阵不兜底召回率；sqlite
  短词走 LIKE 回退。
- mysql 高亮是应用层 indexOf 近似；mysql 同事务 FTS 可见性需实测（不可见则该腿
  index() 改事务外补索引并文档化）。
- `oj_ver_content` 存 db：文本正文场景无损；大二进制版本化将来另议（blob 不可变 key）。
- search/versions 若未来要「不拷模块的首方全局」：升级路径 = 语义原样搬进
  `src/bridge/`，**事务感知照 `op_db_next_seq` 的 `resolve_target`/`Target::Tx` 先例
  搭车调用方事务**（非「直译」），模块 API 不变——语义先验证、落点后升级。
