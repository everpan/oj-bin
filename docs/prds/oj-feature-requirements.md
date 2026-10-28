# oj 功能需求清单:支撑 dbx Web 版方案模块(自包含版)

> 性质:提给 oj 的功能需求,可独立实施,不依赖其他文档。编号 U-* 为本清单内部编号。
> 基线:oj 工作区 2026-10-28(f7f0b5f,oj/Cargo.toml 0.1.91-dev)。文中 file:line 均指 oj 仓自身代码(标注 dbx 仓的除外,dbx 为只读参照库:`/Users/ever/git/only-js/dbx`)。
> 通用验收纪律(适用于全部条目):env-gated 真库套件(`OJ_TEST_MYSQL`/`OJ_TEST_PG` 未设不算绿);错误信封与平台 db 面同形;新 op 纳入 `op_db_*` 前缀白名单守护测试(db.rs:1493-1520);行为变化写 CHANGELOG。

## 0. 背景与协作模型(读懂需求所需的一切)

**dbx** 是一个桌面数据库客户端(参照库,Tauri + Vue,只读)。**dbx Web 版**是基于 ojm 方案包体系把它重建为浏览器端「受控 Web 查询台」:企业站点的管理员登记数据库连接并按角色授权,开发者在浏览器查询。首期仅 sqlite/mysql/postgres 三方言。

**协作模型(决定本清单边界)**:连接的持久化、凭据加密、授权映射、语句门等业务编排全部由方案包(handler + 包内表)承担;oj 提供数据库执行原语、动态连接登记原语与安全门。连接登记后别名直接进 `DB(name)`,复用平台全量查询 op。

**已交付、无需重复实现**(本清单的前提):
- 动态数据源登记六 op:`db.client.register/test/delete/reRegister/list/has`(client_db.rs;登记是特权操作,`db_client.enabled` 默认 false,DSN 过 `secret::redact` 脱敏);登记后别名进 `DB(name)`,白拿 query/exec/stream/tx/listTables/listColumns/listIndexes/listForeignKeys 全量 op。
- L1 访问控制:未在 `db_access.<name>` 显式声明的库,查询与内省一律 fail-closed Deny(db_policy.rs:719-723)。
- L2 结构内省:list_columns / list_indexes / list_foreign_keys(introspect.rs,表名只走绑定参数)。
- 方言能力模型:ABI 12、插件 `describe()` 能力位、结构化错误信封 `{code, retryable, sessionDisposition, data}`(sqlite 内建访问器例外,纯文本)、`plugins.lock.json` 清单锁。
- 客户面中间件插件:oj-kv-redis、oj-bus-kafka、oj-bus-rabbitmq、oj-es、oj-ldap、oj-mail 等(应用业务客户端;注意这与第 5 节 U-15 的「管理面」是两回事)。

**术语**:
- **方案包/包 handler**:ojm 平台的模块形态;handler 是跑在 oj 内的 JS 函数,前端只经 handler 访问后端。
- **别名(alias)**:动态登记的连接名;前端只见别名,不见 DSN。
- **随用随取**:方案包持有连接事实源(包内表);执行路径统一「`db.client.has(alias)` 预检 → 缺则 register 回放 → 使用」,平台侧 client_dbs 只是可随时重建的运行时缓存,重启后首次使用自动恢复。
- **影子轮换**:轮换连接不走 reRegister,而是注册新别名、授权原子切换、旧别名延迟删除(规避 dbCache 陈旧窗,U-3 修复前必须)。

## 1. P0(阻塞 dbx Web 版主旅程)

### U-1 动态库授权运行时生效 + 热更 diff 分类补全
- **现状**:动态别名必须出现在宿主 config 的 `db_access.<name>` 才可查询,否则 Deny;而 `db_access` 冻结在 StableState,热更复用旧 Backend 不重建(app.rs:1524-1531「跳过 assemble_backend」);且 `db_access`/`db_client` 都不在 `diff_configs` 已知键清单(config.rs)里,改这两段热更时既不报「已应用」也不报「待重启」——静默盲区。后果:UI 登记(运行时 op)成功,首查询 Deny,必须改 config + 整机重启才能查。
- **需求**:① 授权 op(建议 `db.client.grant/revoke`,与 register 同族、同特权门、同 fail-closed 纪律),或 `db_access` 支持运行期增量生效;② `db_access`/`db_client` 纳入 `diff_configs` 已知键分类。
- **验收**:register → grant → 首查即通,全程零重启;改 `db_access` 段触发热更时,applied/restart_pending 必居其一并明示;未授权主体查询/内省仍 Deny(不回退)。

### U-3 dbCache 跨 isolate 失效广播
- **现状**:`DB(name)` 实例按 isolate 缓存(bootstrap.js:914);delete/reRegister 只逐出调用方那个 isolate 的缓存(:1095-1100 注释自认)。删除或轮换后,其他 isolate 的新请求仍可能拿到已删除连接。
- **需求**:宿主层失效广播(全 isolate 逐出),或 DB(name) 实例带世代号校验。
- **验收**:isolate A 在用旧连接,isolate B 执行 delete/reRegister 后,A 上新请求拿到新 accessor 或明确失败,绝不复用已删除连接。

### U-6 读侧解码扩展
- **现状**:平台只解六类逻辑类型(整数/浮点/布尔/文本/blob/十进制字符串);mysql 的 `DECIMAL`/日期时间/`JSON`/`BIT`/`GEOMETRY`、pg 的 `numeric`/`json`/`jsonb`/`uuid`/数组/日期时间一律报错并给 cast 指路,无放宽开关;`db.query` 与 `db.stream` 同判据。业务表几乎必有日期时间列,`select *` 必撞。
- **需求**:受限类型读出映射进既有逻辑类型——DECIMAL/numeric → 十进制字符串、日期时间 → ISO 8601 字符串、json/jsonb/uuid/数组 → text、BIT → 位串文本。`db.query` 与 `db.stream` 同步。
- **验收**:三方言各建含 datetime/decimal/jsonb 真业务列的表,`select *` 全列可读;conformance 清单同步;不破坏既有六类行为。

### U-8 sqlite 内建访问器错误信封化
- **现状**:accessor_sqlx.rs 全部错误为 `format!("sqlx query: {e}")` 式纯文本(:43/:178/…/:277),无 BridgeError 包装;JS 面拿到的 Error 无 code/retryable/sessionDisposition 任何附加属性,与 mysql/pg(FFI 信封)不一致。
- **需求**:sqlite 错误包装为与 FFI 面同形的信封(`code` OJ_DB_* 八值 / `retryable` / `sessionDisposition` / `data`)。
- **验收**:sqlite 触发约束冲突/语法错误/文件失败,JS 面拿到结构化字段;消费方可删除纯文本回退分支。

## 2. P0-b(需求确定,验收顺延)

### U-4 tx_close(跨请求事务)
- **现状**:事务存 ReqState,每请求作用域,请求结束 drop = 回滚;FFI vtable 已有 tx_exec 槽但宿主无对应编排 op;「begin → 多次执行 → commit/rollback」无法跨 HTTP 请求持有。
- **需求**:`tx_begin` 返回会话标识;`tx_exec / tx_commit / tx_rollback / tx_close` 按标识路由(归属校验);空闲超时自动回滚;异常/断线路径回收;同一会话并发请求语义写死(串行或拒绝,二选一)。
- **验收**:begin → N 次跨请求 exec → commit/rollback 全链;超时与会话丢失均回滚且可观测;并发语义有测试锚点。

### U-5 execution-id 取消状态机
- **现状**:`op_db_stream_abort`(db.rs:963)只查本请求 `ReqState.db_streams`,跨请求不可达;没有按执行标识的全局取消。
- **需求**:执行标识注册表(租户作用域),`cancel(execution_id)` 跨请求可达,覆盖流式与非流式;与现有流内 abort 并存。
- **验收**:请求 A 发起长查询,请求 B cancel → A 收 cancelled 信封(mysql KILL QUERY / pg pg_cancel_backend / sqlite 声明不支持);重复 cancel 幂等;取消后连接回池。

## 3. GA 门禁(dbx Web 版脱离 beta 的条件)

### U-7 client 轴配额与预算
- **现状**:stream 闸为每 Client 一把(`STREAM_MAX_CONNS=8`,plugins/oj-db-mysql lib.rs:144/:427,pg 同构),即每别名独立 8;`max_entries` 进程级(client_db.rs:84)。真实并发 = max_entries × (池 + 8),无全局预算、无 per-owner 上限、无行数/字节上限。
- **需求**:全局连接预算、per-owner 上限、查询超时、结果集行数/字节上限、拨号超时 + 连续失败计数(防 client_test 沦为内网端口扫描)、拒绝与失败事件落结构化日志。
- **验收**:超限 register/查询被拒且错误可辨识;并发水位压测与 sqlx 池 max_connections × alias 数对账;拒绝事件可按 owner 聚合。

### U-9 审计落存储
- **现状**:SqlEvent(sql_trace.rs:58-82)含 sql/params/operator/risk 等字段但仅 stderr 输出;DB_CLIENT_AUDIT 为结构化 stderr 行(client_db.rs:219-230)。不落存储、无留存策略、无检索、无防篡改。
- **需求**:审计事件(操作者/租户/连接/SQL/行数/成败/耗时)落存储,含留存策略与租户隔离,可检索、可导出;凭据字段零明文。
- **验收**:完整管理员+开发者旅程后可按 user/alias/时间检索;留存到期自动清理。

### G9 服务端防护门(编号沿自上游规划,此处自包含)
- **需求**:在 query/exec op 层强制三条——只读会话位(连接级 read_only,直发 DML 被 op 拒)、写拦截、危险语句确认门(确认 token);验收标准 = 前端绕不过(不经任何包 handler 直接打 op 也被拦)。
- **验收**:三条各自独立实测;错误信封 session_disposition 语义正确。

## 4. 流程

### U-13 补 tag
main(0.1.91-dev)领先最新 tag(v0.1.87)多版。补 tag 后方案包才能钉 `peerOj` 兼容下限(出包前置)。

## 5. P1/P2

### U-11 连接级能力探测
connect 后探测 server_version(mysql `select version()` / pg `show server_version`),per-alias 缓存快照并经 `db.client.list` 透出;`describe()` 工厂级 caps 分派维持不变。

### U-14 元数据目录内省扩展
- **现状**:仅 listTables + 列/索引/FK 三件。数据库客户端还需(参照 dbx 仓 crates/dbx-tauri-schema/commands.rs,68 条内省命令为知识源):库/schema 清单、对象源码/DDL、注释、triggers、constraints(独立于 FK)、partitions、sequences、functions/routines、types、views、行数统计、补全对象清单、linked servers、原生作业。
- **需求**:按 per-dialect 内省契约分三批交付——a 浏览增强(DDL 源码/注释/constraints/统计);b 管理对象(sequences/triggers/partitions/routines/types/views);c 生态(补全对象/作业/linked servers,按需)。全部统一归一 shape 并在 handler `.schema` 声明。
- **验收**:三方言 shape 一致 + golden 快照断言;授权维 granted_only 贯穿;未知方言报 unsupported 不静默回退。

### U-15 非 SQL 管理 connector 轴
- **背景辨析**:oj 现有中间件插件(redis-kv/bus-kafka/bus-rabbitmq/es)是**客户面**(应用连中间件做业务);数据库客户端需要的是**管理面**(DBA 运维:进程列表、KILL、配置读写、主题/消费组管理、集群操作)。两者语义不同,复用客户面插件承载不了。
- **需求**:承载管理面语义的 connector 体系(mongo、redis 管理、MQ 管理、配置中心 nacos/consul/etcd/zk),与客户面插件正交;复用既有 sidecar 生命周期契约(清单锁/指纹/能力位/降级/存活探测)。首个 connector 建议 mongo。
- **验收**:首个 connector 走通 登记 → 管理操作 → 审计 → 配额 全链;能力位声明未实现即报 unsupported。

### U-16 服务端 SSH 隧道(条件项)
仅当产品取舍(「客户端不触达跳板机后的库」)被推翻才立项。形态:client_register 增加隧道前置参数 + 服务端隧道管理。

## 6. 远期/显式不做(登记备查,不排期)

- **U-10 运行时凭据轴**:运行期 seal/open op 或凭据服务(连接凭据加密存储/轮换/会话级密码)。现状:secrets 信封仅配置解析期(config.rs:1500-1515)+ CLI(secret_cmd.rs:120-123),serve 运行期无任何 seal/open 路径。实现前提四条:公钥装配期加载、解封闸扩展至 client 清单、解封失败逐 alias degrade + 启动横幅点名、无 keypair fail-closed 拒 register。
- **U-12 LOB 轴**:FFI vtable 16 槽(oj-plugin-ffi/src/db.rs:10-52)无 LOB 槽位,需 ABI bump + read_chunk/release/download op。参照 dbx 桌面仓 query.rs 的 valueRef 语义:后端持大对象间接引用,前端分块读、显式释放、可下载落盘。
- **PL/SQL 调试协议**(断点/变量/栈/编译):独立协议工程,显式不做。
- **Oracle/达梦方言**(上游 M2 计划):开工门禁未过(达梦 DPI 许可书面结论、Go 工具链进 CI),本清单不设时点。

## 7. 明确不需要 oj 实现的(边界声明,防范围蔓延)

- **导出格式生成**(XLSX/HTML/JSON/MD):前端 JS 生成 + 浏览器下载;CSV 由方案包 handler 基于现成 `db.stream + json.stream` 范式实现。
- **SQL 文本函数**(语句切分/光标定位/引用分析/SQL 构建器,dbx 侧约 67 条):前端库平移,零后端调用。
- **AI 代理 / MCP**:方案包 handler 走 `http/fetch + tasks` 即可,无平台轴需求。
- **非 SQL 客户面**(应用连 redis/es/kafka 做业务):插件已交付;缺的只是 U-15 管理面。
- **应用插件体系、驱动运行时安装、桌面窗口/更新/密钥链**:淘汰或由方案包平台生态/部署期承接。
- **连接持久化**:已裁决由方案包自管——包内表事实源 + register 回放随用随取(原需求「client_dbs 持久化」撤销)。oj 只需保证 register/has/delete 语义稳定且行为与本文 U-3 一致。

## 8. 汇总

| 档 | 条目 | 数量 |
|---|---|---|
| P0(阻塞主旅程) | U-1、U-3、U-6、U-8 | 4 |
| P0-b | U-4、U-5 | 2 |
| GA 门禁 | U-7、U-9、G9 | 3 |
| 流程 | U-13 | 1 |
| P1/P2 | U-11、U-14、U-15、U-16(条件) | 4 |
| 远期/不做 | U-10、U-12、PL/SQL、Oracle/达梦 | 4 |
| **oj 需实现合计** | | **14 项功能 + 1 项流程** |
