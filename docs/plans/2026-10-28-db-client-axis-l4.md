# L4：动态数据源登记的实例化能力（client 轴，dbx 对齐）

> 状态：**已四路评审 + 已实施（v0.1.92）**。
> 落盘方案经四路评审初定「default 库 `oj_client_db_registry` 表写穿 + 回放」，但**实施前用户最终拍板收窄范围**：
> **框架只提供 DB 实例的「实例化能力」，落盘持久化交给应用侧自己做**——故原 `ClientDbStore` 表写穿 /
> 回放 / `db_client.persistence` 配置**不落地**，本版最终交付为「实例化 + 生命周期 API 稳定化 + 应用侧
> 持久化配方」。
> 上游基线：`f7f0b5f` feat(db): 结构内省增强 + 动态数据源登记（L2/L3）
> 目标版本：v0.1.92

## 0. 背景与范围

L3（v0.1.91）把动态数据源登记做成了**纯内存**结构（`Arc<ClientDbRegistry>` 内部 `Mutex<HashMap>`），
提供 `db.client.register` / `reRegister` / `delete` / `list` / `has` / `test` 六个 op——这就是 **DB 实例的
实例化与生命周期 API**：

- `register(name, dsn)` = 以 `(name, dsn)` **构造一个 DB 实例实体**并挂入命名注册表，随后 `DB(name)` 即可用；
- `reRegister(name, dsn)` = 替换某实例的底层连接；
- `delete(name)` = 销毁实例并驱逐 `DB()` 缓存；
- `list` / `has` / `test` = 探活与审计面。

dbx 侧是把连接持久化到本地 SQLite + 加密密钥（`crates/dbx-core/.../storage.rs` + `secret_codec.rs`
AES-256-GCM 信封），使「运行期增删的连接」跨重启存活。

**本 L4 的建模视角（用户拍板）**：被管理的是 **DB 实例实体**——`db.client.register(name, dsn)` 即
`db = new DB(name, dsn)` 创建实例；框架负责**提供实例化能力**，而**落盘持久化是应用模块自己的事**：
应用若需要「重启后登记仍在」，应在**自己的表里存 `(name, dsn)`**，并在 `oj serve` 启动后的初始化 handler
里逐条 `db.client.register` 重建。框架**不**替应用决定存储介质、加解密、备份策略——那是应用关切。

**做：**

- 把 `db.client.register`（含 `reRegister` / `delete` / `list` / `has` / `test`）正式确立为 **DB 实例的
  实例化 / 生命周期 API**；其语义、错误码、访问控制（L1 `db_access`）+ 内省（L2）复用关系在文档中落定。
- 在对外文档给出**应用侧持久化配方**：应用自存 `(name, dsn)`（`ENC[...]` 密封 DSN 复用既有凭据范式），
  启动初始化时回放 `register`。

**不做（范围收窄，用户拍板）：**

- 不引入框架级落盘：不做 `default` 库 `oj_client_db_registry` 表写穿 / 回放、不做 JSON 文件 + HMAC、
  不引入外部 KV 作为框架存储后端。
- 不加 `db_client.persistence` 配置段、不加 `ClientDbStore`、不在 `StableState` 增 `client_store`。
- 不改 ABI / 不改 `db_client` 六 op 的 JS 契约（L3 已冻结）。

## 1. 设计

### 1.1 实例化模型（框架侧交付）

```
register(name, dsn)
   └─> DbBackendRegistry.connect(dsn)   // 构造活连接（锁外完成，不持锁跨 await）
        └─> ClientDbEntry{ accessor, dsn_redacted, dialect, registered_by, registered_at_ms, queries, last_error }
             └─> client_dbs.register(name, entry)   // 壳 Arc<ClientDbRegistry> 不可变，内部 Mutex 变动
                  └─> DB(name) 立即可用（复用 L1 访问控制 + L2 内省 + 全 query/exec/stream）

delete(name)        └─> client_dbs.remove(name) + dbCache.delete(name)   // 销毁实例 + 驱逐缓存
reRegister(name,dsn)└─> 同 register 但要求 name 已存在
```

- 实例是**进程的、内存的**：进程退出 / `kill -HUP` 全量重建后，未由应用重建的实例丢失（与 L3 一致）。
- 持久化的是**连接定义**还是**活连接**？框架只持有内存实例；若应用要跨重启保留，由应用持有 `(name, dsn)`
  并在启动时重建——框架不保存任何 DSN 明文/密文。

### 1.2 应用侧持久化配方（推荐做法）

应用需要耐久登记时，自行负责落盘与回放：

```js
// 1) 登记时：构造实例 + 应用自己落盘（dsn 经 secret.seal 密封，绝不存明文）
const enc = await secret.seal(dsn);                 // 复用既有 ENC[...] 凭据范式
await db.query("insert into app_db_registry(alias, enc_dsn) values (?, ?)", [name, enc]);
await db.client.register(name, dsn);                // 框架实例化

// 2) 删除时：销毁实例 + 应用自己删行
await db.client.delete(name);
await db.query("delete from app_db_registry where alias = ?", [name]);

// 3) 启动初始化（某 init handler / 启动钩子）：读行解封 + 重建实例
const rows = await db.query("select alias, enc_dsn from app_db_registry");
for (const { alias, enc_dsn } of rows) {
  const dsn = await secret.open(enc_dsn);
  try { await db.client.register(alias, dsn); }
  catch (e) { log.warn(`skip re-register ${alias}: ${e}`); }   // 单条不可达不 brick 启动
}
```

- 存储介质、表结构、加解密、备份全是应用关切；框架不感知。
- 多实例共享：若多个 `oj serve` 都连同一业务库并各自回放，登记在各自进程内存——它们之间不共享，但都指向
  同一真实 DB，语义上一致（DB 实例本就是 per-process 的连接池）。

### 1.3 与既有语义的交互

- **授权不持久化**：`DB(name)` 能否用仍由 config 的 `db_access.<name>` 决定（未声明 → Deny，L3 已钉死）。
  应用自存的只是连接定义，授权仍走 L1。
- **热重载（notify）**：沿用 L3 的 `Arc<ClientDbRegistry>` 同一实例，内存登记在热重载间保留；只有**全量重建**
  （HUP/重启）才丢失——除非应用自行回放。
- **生命周期语义**：`register` 是「构造 + 挂表」，不是「持久化承诺」。需要耐久 = 应用负责落盘 + 回放。

## 2. 安全红线

- 框架侧：落盘 DSN 的脱敏（`db.client.list` 的 `dsn` 全经 `secret::redact`）与 L3 一致；`asSystem()` 不豁免
  登记门、未声明 `db_access.<name>` 的动态库默认 deny——这些**不变**。
- 应用侧：若应用自行落盘 DSN，必须 `ENC[...]` 密封（复用 `secret.seal`），**绝不存明文**；解封失败
  （无 `OJ_SECRET_KEY`）应 fail-loud，不当明文用。

## 3. 测试

实例化 / 生命周期的行为测试已在 L3 的 `src/bridge/client_db.rs` 测试覆盖（register/overwrite/delete/
reRegister/delete-evicts-cache/list/has/alias-validation/access-control）。本 L4 不新增框架持久化测试
（无框架持久化代码）；应用侧配方由应用自行测试。

## 4. 文档同步

- `docs/db-guide.md` §11.6：明确「实例化 + 生命周期」语义 + 应用侧持久化配方。
- `docs/devkit/api-manual.md` §6 `db.client`：标注 `register` 即实例化原语；补「框架不落盘，耐久由应用负责」陷阱。
- `docs/devkit/scenarios.md` 场景 42：加应用侧持久化配方。
- `docs/devkit/SKILL.md` 陷阱速查：框架不持久化、需应用启动时 register 重建。
- `docs/devkit/README.md` 版本同步段：v0.1.92 落「实例化能力（持久化应用侧负责）」。
- `CHANGELOG.md` v0.1.92。

## 5. 四路评审与最终拍板

§6 记录的四路评审初版结论是「有条件通过，落盘介质 = default 库表」。实施前用户最终拍板：

- **落盘不进框架**：框架只提供实例化能力；持久化介质 / 加密 / 备份由应用侧决定。
- 因此 §6 中「default 库表写穿 / 回放」「`db_client.persistence` 配置」「`ClientDbStore`」等结论**作废**，
  改为 §1.2 的应用侧持久化配方。安全 B1（防篡改签名）在原「JSON 文件」方案下成立，但在「框架不落盘」模型下
  **不适用**（框架不持有任何登记持久化文件/表）；应用若落盘，应按 §2 自担机密性。
