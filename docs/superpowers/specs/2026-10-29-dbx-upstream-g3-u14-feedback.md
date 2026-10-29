# U-14（v0.1.109）需求反馈——下游 check 用

> 面向 dbx 侧（PR-2 DDL 查看 / PR-6 / PR-7 结构编辑 / PR-9 结构对比）逐条对照
> `2026-10-29-dbx-upstream-g3-u14-design.md` 的核查清单。
> 状态：✅ 按 spec 实现　⚠️ 偏差（有裁决，行为以本文与 devkit 文档为准）　🔧 终审修复增补　⏸ 延期（下版）

## 一、契约偏差（最重要，PR-9 直接受影响）

### ⚠️ B1：`diff` 形状 = 嵌套四维（spec 草图是扁平键）

spec 草图写扁平键（`addedColumns/removedColumns/changedColumns/…`），实现与全部发布物
（`global.d.ts` 的 `DdlDiffResult`、api-manual 错误/限制表、db-guide §3.3、场景 57）一致使用
**嵌套形状**：

```ts
diff: {
  columns:     { added: string[]; removed: string[]; changed: {name,field,from,to}[] },
  constraints: { …同上… },
  indexes:     { …同上… },
  foreignKeys: { …同上… },
}
```

- **裁决**：嵌套形状为发布契约（四件发布物 + 类型面已一致，改动成本高且嵌套更贴合 PR-9
  按维度渲染）；spec 草图视为早期示意。spec 原文已同步标注。
- **下游 check**：PR-9 比较视图按 `diff.<dim>.{added|removed|changed}` 取数，**勿按 spec
  草图的扁平键**。
- 建表路径（`from = null`）diff 同此形状（全 added）——终审修复 6 对齐，无特例分支。

### ⚠️ B2：`changed` 的 `field` 枚举比 spec 草图多两个生效值

spec：`nativeType/nullable/default/identity/comment/position`。实现另有两个**防御性**值：
`type` / `definition` / `unique` / `columns` / `refTable`（出现在 constraints/indexes/
foreignKeys 维度的 changed 条目里，列维度的 `columns` 数组差也会以 `columns` 字段名出现）。
下游渲染 changed 列表时按 `field` 白名单映射列标题，未识别的 field 请回退显示原名。

## 二、正向 DDL（`db.catalog("ddl", name)`）

| # | 需求 | 状态 | 核查点 |
|---|---|---|---|
| 1 | MySQL SHOW CREATE 激活（拦截前置） | ✅ | 真机 MySQL 8.4：输出含 `CREATE TABLE`+`AUTO_INCREMENT`；**输出含 `AUTO_INCREMENT=<当前值>` 计数器**，固化前剥 |
| 2 | scoped × MySQL ddl → `OJ_DB_UNSUPPORTED(for_schema)`，先例顺序保持 | ✅ | 真机腿钉死（先拦 scoped 再 SHOW） |
| 3 | relkind 路由矩阵 v/m→viewdef（逐字节不变）、r/p→拼接器、其余→`items:[]` | ✅ 🔧 | 静态守卫+真机腿；'p' 终审补 `PARTITION BY`（见 #7） |
| 4 | 拼接器查询集 | ⚠️ | spec 写「探测+4 查询（注释合一）」，实现为 **6 常量**：列/约束/索引/列注释/表注释（后两者拆分）+ 分区键（终审增补）。行为等价，容量/超时口径不变 |
| 5 | 容量防线 `OJ_DB_CAPACITY`（禁静默截断） | ✅ | `capacity()` 单构造器，6 查询统一检查 |
| 6 | 30s 整体 deadline | ✅ | `STITCH_BUDGET_MS = 30_000` 包全部查询 |
| 7 | 分区表 `pg_get_partkeydef` best-effort | ✅ 🔧 | 初版遗漏（假 DDL 风险）→ 终审修复：ddl 附 `PARTITION BY <def>`（真机 PG16 腿）；**子分区仍不产出**，跨库重放须自行换索引名（同 schema 唯一） |
| 8 | serial 走 pg_depend 'a' + `OWNED BY`；identity 渲染 `GENERATED … AS IDENTITY` | ✅ | 真机往返重放结构一致（nextval default 归一 `<seq>` 对账） |
| 9 | 约束含排他 'x'；排除约束支撑索引（conindid） | ✅ | 静态守卫钉死，无 `indisprimary` 粗过滤 |
| 10 | 已知限制登记（快照 best-effort/Pool 臂异连/跨 schema FK 不可移植/Tx 毒化） | ✅ ⏸ | 四件套已写；**超时错误文案未点名 Tx 毒化**（延期 minor，db-guide 已有文字条款） |

## 三、反向 DDL（`db.ddlDiff`）

| # | 需求 | 状态 | 核查点 |
|---|---|---|---|
| 1 | 纯渲染零 DB I/O；`granted_only`；执行走 `db.exec`+`allow_ddl` | ✅ | op 边界单测 + sqlite unsupported 腿 |
| 2 | 缺省 = 不参与 / 空数组 = DROP 全量 | ✅ 🔧 | 初版**列维度破例**（缺省端当空表 → 全量 ADD/DROP COLUMN，毁表面）→ 终审修复，四维统一 |
| 3 | 语句排序 DROP(FK→约束→索引→列)→列变更→ADD→约束/索引→FK 最后 | ✅ 🔧 | 单测钉死；**同名 changed 维度先 DROP 再 ADD**（终审修复 2——spec 未写，重放依赖此行为） |
| 4 | MySQL 约束 DROP | 🔧 | spec 未规定语法；实现按类型分派：pk→`DROP PRIMARY KEY`、uq→`DROP INDEX`、ck→`DROP CHECK`（终审修复 3） |
| 5 | identity 跳过 default 文本比对 | ✅ | identity 恒等时生效（重写后的单测钉死） |
| 6 | identity 出现在 changedColumns | ✅ 🔧 | 初版遗漏（假 diff）→ 终审修复：翻转出 `field:"identity"` + PG `ADD/DROP IDENTITY`；**identity 为 PG 专属，MySQL 结构里出现 → 参数错误**（不产非法 SQL） |
| 7 | 注入防线：方言引号+翻倍（非白名单）；NUL 拒；verbatim 拒 `;` | ✅ 🔧 | 终审修复 5 补列名/索引成员名 parse 期校验（初版只校表名/具名维度名） |
| 8 | MySQL 位置差 `AFTER`/`FIRST`；PG 忽略 | ✅ | 单测钉死（PG changed 无 position 项） |
| 9 | 不生成序列对象（serial 的 SEQUENCE 不在 ALTER 流） | ✅ | SKILL 陷阱已登记 |
| 10 | sqlite → `OJ_DB_UNSUPPORTED(ddl_diff)` | ✅ | 真机腿 + 单测 |
| 11 | 版本错位能力探测 `typeof db.ddlDiff === "function"` | ✅ | global.d.ts 注释 + 场景 57 |
| 12 | MySQL DDL 隐式提交 / PG `USING col::type` / PG 无列位置 | ✅ | api-manual 错误表 + db-guide §3.3 |

## 四、⏸ 延期 minor（下版处理，下游须知）

1. **PG 新建列/建表路径的列注释不落 DDL**（`COMMENT ON COLUMN` 只在 changed 路径产出）——
   add-column-with-comment 场景注释静默丢失。
2. **`diff.<dim>.removed` 在维度缺省时列 from 全量**（statements 正确地未出 DROP）——
   PR-9 比较视图若直接渲染 removed 会得到「待删除」假信号；**过渡方案：diff 与 statements
   对照使用，或忽略缺省维度的 removed**。
3. 拼接器超时错误文案未点名 Tx 臂毒化（db-guide 文字条款先行）。
4. `db_ddl.rs` 尾部一处重复注释块（cosmetic）。
5. MySQL `catalog("ddl")` 空/缺 arg 现报 generic「invalid identifier for SHOW CREATE」
   （旧行为是结构化 unsupported(capability=ddl)）；非 ASCII 合法表名被 SHOW 白名单拒
   （白名单例外本身已文档化）。

## 五、解锁状态对照（spec 交付顺序表）

| dbx PR | spec 承诺 | 实际 | 备注 |
|---|---|---|---|
| PR-1 执行计划面板 | v0.1.108 解锁 | ✅ v0.1.108 已发布 | G3 `db.explain` |
| PR-2/PR-6 DDL 查看 | v0.1.109 解锁 | ✅ | 正向两方言；分区表附 PARTITION BY |
| PR-7 结构编辑（G9 写回） | v0.1.109 解锁 | ✅ | ddlDiff statements 逐条 `db.exec`+`allow_ddl` |
| PR-9 同方言结构对比 | v0.1.109 解锁 | ✅（带 B1/B2 契约 + minor 2 过渡方案） | 跨库对比 ✗ 另立（类型等价类独立工作项，维持原判） |
| PR-8 导入向导 | 不解锁（REQ-03 为准） | ⏸ 未动 | 勿误读 |

## 六、复测指引（下游 smoke）

```bash
# 引擎侧（本仓库）
OJ_TEST_PG=postgres://… OJ_TEST_MYSQL=mysql://… cargo test --release -p only-js ddl_diff
OJ_TEST_PG=postgres://… cargo test --release -p only-js catalog_ddl

# dbx 侧建议探针
typeof db.ddlDiff === "function"            // 版本错位探测（勿版本号嗅探）
await db.catalog("ddl", "<table>")          // 正向：PG 拼接口径 / MySQL SHOW 口径
await db.ddlDiff({ objectType:"table", name, from, to })  // 反向：diff 用嵌套形状（B1）
```
