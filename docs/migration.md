# 迁移手册（Migration Runbook）

面向模块数据层的日常运维：声明式 schema（`schema.yaml`）、手写迁移（`migrations/`）、
迁移账本、`oj migrate` / `oj schema diff`、表归属守卫。规则背景见 `user-manual.md` §5，
部署排障见 `ops-manual.md` §7。

## 1. 心智模型

- **声明为源**：每模块可选 `schema.yaml` 声明自己拥有的表。启动 / `oj migrate` 时自动
  收敛到声明（**安全前向**：缺表 CREATE、缺可空列 ALTER ADD、缺索引 CREATE INDEX）。
- **演进靠迁移**：无法安全推导的变更（NOT NULL 列新增、疑似改名、类型变更、数据回填）
  一律 fail-fast 并打印迁移模板——手写 `migrations/{seq:04}__{desc}[.{sqlite|mysql|postgres}].sql`。
- **账本记账**：`_oj_migrations_<module>` 记录每模块已应用的迁移；带方言后缀的文件只在
  对应方言库执行。
- **表归属**：schema.yaml 喂归属图（表 → 模块，双声明拒启 S002）与 `SchemaRegistry`
  （`db.table()` 列白名单）；跨模块表访问须 manifest `deps:` 声明（静态 S003 + 运行时
  `ownership_guard`）。

## 2. 场景 → 操作

| 场景 | 操作 |
|---|---|
| 新环境首次部署 | `oj build` → `oj migrate -c config.yaml -d dist` → `oj server`（release verify 门禁要求先迁移） |
| 加新表 / 可空列 / 索引 | 只改 `schema.yaml`，下次启动或 `oj migrate` 自动收敛 |
| 加 NOT NULL 列 | `schema.yaml` 声明 + 手写迁移（`ALTER TABLE … ADD COLUMN … NOT NULL DEFAULT <值>`）——reconcile 对存量行无值会 fail-fast 并打印模板 |
| 改列名 / 删列 | 手写迁移（`RENAME COLUMN` / 先备份后 `DROP`）；删除列同时从 schema.yaml 移除，`oj schema diff` 会把残留报为 D001 多列 |
| 存量库接入（表已存在） | `oj migrate --baseline`：≤head 的迁移全部记为已应用而不执行；声明与实库的差异用 `oj schema diff` 核对后对齐 |
| 方言差异 | 写 `0002__add_x.mysql.sql` 等方言文件，与本方言文件并存；无后缀 = 全方言执行 |
| 演示/测试数据 | `fixtures/` 仅 `oj test` / `oj fixture` 灌入，不进发布产物；`seed.sql` 幂等随启动重放（S006 校验） |
| 发布前巡检 | `oj schema diff`：D001 声明 vs 实库漂移、D002 实库有而未声明的表；有漂移 exit 1 |

## 3. 门禁与守卫

- `server.migrate_on_start`：`auto`（dev 默认，启动即应用）/ `verify`（release 默认，
  账本落后拒启 M004，报错附 `oj migrate` 命令）/ `off`（迁移完全归运维）。
- `server.ownership_guard`：`warn`（默认，跨模块表访问仅告警）/ `deny`（未声明 deps
  拒绝执行，500 附修复指引）。灰度建议：先 `warn` 观察日志，声明补齐后切 `deny`。
- **无主表语义**：归属图取自部署目录内各模块 schema.yaml——模块未部署时其表无主，
  运行时不设防（部分部署/灰度属设计语义），构建侧 S003 兜底。

## 4. 检查规则速查

| 层 | 规则 | 触发点 |
|---|---|---|
| 结构 S001–S007 | manifest 合法性 / 表归属单射 / 跨模块依赖声明 / deps 版本范围 / tables 与 schema.yaml 一致 / seed 纪律 / 迁移文件序列 | `oj build` 内建（违规 fail build）；`oj build --check` 只查不落盘（CI 门禁） |
| 漂移 D001/D002 | 缺表/缺列/多列/缺索引；实库未声明表 | `oj schema diff`（只读，漂移 exit 1） |

误报逃生门：SQL 注释 `/* oj:allow-table=x,y */`。

## 5. 回滚

- **应用回滚**：换回上一版 `dist/`（`dist/manifests.yaml` 指回旧版本目录 + 重启）。
- **schema 回滚没有自动机制**：迁移只前向（refinery 语义）。破坏性变更前备份
  `*.sqlite` / mysqldump / pg_dump；手写回填迁移（如 `RENAME` 回去）作为新 seq 前向执行。
- 账本与迁移文件的一致性由 S007 把守：文件名空洞、乱序、desc 与账本不一致 → 报错。
