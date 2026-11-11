# 表配置覆盖层（`_oj_config`）设计

日期：2026-11-11
状态：待评审
前置：`server.reload_api` 热更管线（spec 2026-10-12，v0.1.122）已落地。

## 1. 意图与背景

配置现状为纯文件（`config.yaml`）：改配置要登录部署机编辑文件，再（历史上要重启；
现在可 `POST {base}/_system/config` 热更）。痛点：**改配置依赖 SSH**，且热更后
`GET {base}/_system/config` 只回代际摘要（generation/sha/modules），**看不到最终生效
的配置内容**，无法核对。

目标：

1. 管理员通过 web 端（sample admin 页面）写 `_oj_config` 表 → 手动触发
   `POST _system/config` → 配置生效，全程不登录部署机。
2. `GET _system/config` 回**最终合并配置**（脱敏）供核对，并标注每键来源与
   `restart_pending` 项。

## 2. 裁决（用户已定 / 我的裁决）

| # | 裁决 | 来源 |
|---|------|------|
| 1 | 触发**仅手动**：改表后调 `POST _system/config`（或 JS `server.reloadConfig()`）；**不做轮询任务**。多实例需逐实例触发（文档写明） | 用户 |
| 2 | 表写入走 **sample admin handler**（isAdmin）；core 对表**只读** | 用户 |
| 3 | 后端轴（db/kv/blob/bus/es/mail/ldap 连接与插件 cfg）改动沿用既有 `restart_pending` 分类，**v1 不热重建**（`do_reload` 复用旧 Backend 是既有语义） | 用户未反对，沿用现状 |
| 4 | overlay 读取方式：全量 `SELECT`（行数 = section 数，量小）；摘要/stamp 用聚合（`COUNT(*)`/`MAX(updated_at)`；GET 摘要按 `GROUP BY section` 展示每段 stamp）——用户原话「采用 select group by 获取」按此理解，如意图不同请在评审时指出 | 我的解读 |
| 5 | 表名 `_oj_config`，**每 section 一行**（section PK，UPSERT 更新）；不做 append 历史（审计后续再说） | 我的裁决 |
| 6 | GET `_system/config` **不**脱离 `reload_api` 常开（常开意味着 plain 分支加路由；既然触发本身依赖 reload_api=true，核对端点随之存在即可） | 我的裁决 |
| 7 | boot 时 `_oj_config` 不存在 → `CREATE TABLE IF NOT EXISTS` 自动建（三方言 DDL 差异小）；建表/读表失败或行非法 → **fail-fast**（house style）；reload 时 overlay 非法 → 400 保持旧代（现状语义） | 我的裁决 |

## 3. 方案总览

**effective 配置 = 文件 doc 深合并表 overlay**。合并发生在 raw doc 层（ENC 解密
之前），因此：

- `ENC[...]` 沿用 `load_cfg` 既有解密管线：管理员本地持**公钥** `oj secret seal`
  出密文贴进表 → 部署机 reload 时解密。**私钥不出部署机**红线不破。
- sha 幂等闸、admission 门禁、`diff_configs` 三清单（applied / restart_pending /
  kept_listener）、影子装配、原子 swap **全部白得**——overlay 变更即 sha 变更，
  既有管线无感知。
- 每代 `do_reload` 重建 App/桥 → **`vars:` 段的 overlay 变更是热生效的**（applied
  类）。运行期可调参数（开关、阈值、feature flag）应放 `vars:`，这是本设计的主通道。

```
boot / do_reload：
  读 config.yaml → parse raw doc
  → 取 overlay：SELECT section, value_json, updated_at, updated_by FROM _oj_config
  → 深合并（§5）→ 合并后 doc
  → doc_sha（幂等基线，含 overlay）/ load_cfg（解密+归一+校验，含 overlay）
  → admission_gate → 影子装配 → swap
```

## 4. 表

```sql
CREATE TABLE IF NOT EXISTS _oj_config (
  section    TEXT PRIMARY KEY,   -- config 顶层段名：vars / db_query / db_trace / …
  value_json TEXT NOT NULL,      -- 该段的 JSON 覆盖值（对象或标量）
  updated_at TEXT NOT NULL,      -- ISO8601
  updated_by TEXT NOT NULL DEFAULT ''
);
```

- core 建表与读取走 sqlx 直连（core 不经 SchemaRegistry）；JS admin CRUD 走查询
  构造器 → 表须在 `_platform/schema.yaml` 声明白名单（系统表先例：`oj_versions`）。
- 每行 = 一个 config 顶层段的覆盖值。行缺失 = 该段回落文件值（**删行即还原**）。

**禁改段**（overlay 出现即报错，boot fail-fast / reload 400 点名段名）：

- `db` 的引导条目 `default` —— 它正是用来连库读表的（鸡生蛋）；其余 `db:` 别名可覆盖（restart_pending）。
- `secrets` —— 私钥路径是本机文件概念，远程改它等于把解密能力悬空。
- `plugins_dir` / `mounts` —— 指向部署机本地目录，远程改无意义（错误路径会被 admission 拦，但语义上禁掉更清晰）。

## 5. 合并语义

对 config 顶层段的覆盖值递归深合并进文件 doc：

- 对象 ∩ 对象 → 递归合并；其余情况（类型不一致）→ 覆盖值整体替换。
- **数组整体替换**（不按下标合并）。
- 覆盖值为 `null` → **删除**文件 doc 中的对应键（用于「还原文件里多余的键」；对
  必填键删除会在 load_cfg 校验期自然 fail-fast，属预期）。
- overlay 里出现禁改段/禁改键 → 整体拒绝（见 §4），不做静默剥离。

## 6. 读取端点增强（GET `_system/config`）

现有响应（generation/sha/modules/清单）之上追加：

```jsonc
{
  // …现有字段…
  "effective":  { /* 合并后 raw doc；ENC[...] 密文原样可见，非 ENC 敏感叶打码 */ },
  "overlay":    { "<section>": { "value": …, "updated_at": …, "updated_by": … } },
  "overlay_stamp": { "rows": 3, "max_updated_at": "…" }   // 聚合/GROUP BY 口径
}
```

- **脱敏**：`effective` 递归遍历，键名命中敏感模式（`password`/`passwd`/`secret`/
  `token`/`access_key`/`secret_key`/`private_key`）或键位于 `db:`（DSN 含凭据）的
  **非 ENC 值** → 替换为 `"******"`；`ENC[...]` 密文原样返回（密文可核对「哪个值
  已密封」，无私钥不可读，不泄密）。
- GET/POST 鉴权沿用 `auth_guard.verify` 现状；GET 现在携带配置内容，**必须非匿名**
  （现有管线已要求 Bearer，保持即可；如现有实现允许 anonymous_paths 命中 `_system`，
  须显式排除——builder 核查）。

## 7. 写入端（sample admin，core 不参与）

`sample/src/admin/config/api.ts`（isAdmin 模式现成）：

- `GET /v1/api/admin/config/` — 列 sections + 每段 stamp（GROUP BY 聚合）。
- `GET /v1/api/admin/config/<section>/` — 读一段 overlay。
- `PUT /v1/api/admin/config/<section>/` — UPSERT（body=JSON 值 + updated_by 取
  `http.user`）；写前本地预检禁改段/禁改键，命中即 400。
- `DELETE /v1/api/admin/config/<section>/` — 删行 = 回落文件值。

前端（admin 页面）流程：改表 → `POST {base}/_system/config` → GET 核对 effective +
三清单。**多实例**：逐实例 POST（实例地址清单属部署拓扑知识，不进本设计）。

## 8. 实现落点

- `oj/src/serve_cmd.rs`：`load_cfg` 重构为可吃合并后 doc（或新增
  `effective_doc(config_path) -> (doc, sha)`：读文件 → 连库取 overlay → 深合并）；
  boot 分支与 `do_reload` 都改走它。overlay 读取连库用引导 DSN（文件里的
  `db.default`，**不经 overlay**）开短连接（reload 频率为人操作级，成本可忽略）。
- `serve/src/reload.rs`：GET summary handler 增强（§6）+ 脱敏函数。
- 脱敏/合并函数放根 crate（`src/config.rs` 或新 `src/config_overlay.rs`），oj 与
  serve 都可见，单测落根 crate。
- `sample/src/_platform/`：`schema.yaml` 声明 `_oj_config`；`migrations/` 增补
  DDL（与 core boot 自动建表幂等共存：都用 `IF NOT EXISTS`）。
- `sample/src/admin/config/`：§7 handler。

## 9. 测试

- 单测（根 crate）：深合并语义（嵌套/数组替换/null 删除/类型冲突/禁改段拒绝）；
  脱敏（模式命中、db 段、ENC 原样）；`effective_doc` 对坏行的 fail-fast。
- 集成（env-gated，本机 PG/MySQL 容器，见记忆 `local-test-db-containers`）：
  boot 带 overlay 生效；POST 触发后 applied 类（`vars:`）热生效；后端段变更 →
  `restart_pending`；删行还原。
- e2e（`oj/tests/e2e.rs`）：admin handler CRUD + 触发 + GET 核对全链路。

## 10. 文档红线

- CHANGELOG.md + 版本号推进。
- devkit 四件对齐：`api-manual.md`（`GET _system/config` 新字段、admin config API、
  错误文案进错误/限制表）、`scenarios.md`（远程改配置场景可照抄）、`SKILL.md`
  （陷阱：多实例要逐实例 POST、禁改段清单、null 删除语义）、`README.md`；
  `cargo xtask build` 归置 + `cargo test --release -p xtask` devkit 契约。
- `docs/dev-guide.md` 热更章节与 `docs/oidc-integration.md` 无关不动。

## 11. 非目标（v1 不做）

- 后端轴热重建（连接池重建 + 插件二次 init 幂等性未验证，需先 spike）。
- 轮询自动发现 / bus 广播失效。
- 配置变更历史表、diff 回滚 UI。
- core 写端点（`PUT _system/config`）。
- admin 前端页面本身（本设计只定 API；页面属 sample 前端工作）。
