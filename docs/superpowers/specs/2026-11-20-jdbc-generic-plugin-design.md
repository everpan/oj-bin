# oj-db-jdbc 泛型 JDBC 插件设计（v1.2）

日期：2026-11-20 · 状态：待审批 · 参考：`ref/dbx`（Apache-2.0）的 JDBC sidecar 适配思路

> v1.2：四角色评审（架构/工程/产品/数据库）后修订，全部重点主张已经本人对照宿主代码
> 二次核实。主要变化：宿主「零改动」修订为「一行门控补漏」（db.import panic 路径，
> ffi.rs:428 + db_import.rs:1964 已核实）；类型映射按达梦/Oracle 驱动实测语义重写
> （setNull/scale 分流/LOB 上限/时区口径）；sidecar 生命周期补全（stderr drain、
> EOF 退出契约、per-conn 串行化队列）；ENC 整串密封确立为 config 静态 DSN 的正道；
> 能力串撤下 id_returning；测试计划加 H2 开箱腿与 Rust 假 java 跨平台夹具。

## 1. 意图与为什么

oj 的 db 轴目前只有 mysql / postgres / sqlite 内置后端。大量企业库（Oracle、达梦、
人大金仓、Hive 等）官方只提供 JDBC 驱动，Rust 原生驱动要么不存在要么质量差。参考 dbx
的做法：**Rust 不直接碰 JDBC，而是孵化一个 Java sidecar 进程，通过 newline-delimited
JSON（NDJSON）over stdio 收发请求**，Rust 侧插件只做进程管理与协议翻译。任何「丢 jar
进目录」的 JDBC 驱动都能接入，一个插件覆盖全部 JDBC 数据库——这就是"泛型"的含义。

### 用户已拍板的事

1. **JRE 策略**：依赖外部环境 Java。插件装配时不强制；首次 connect 才探测 `java`，
   缺失给出清晰报错（不静默降级）。
2. **首版 e2e 驱动**：达梦（远程实例实测，主腿）+ 人大金仓（镜像可得时）。Oracle 不在
   v1 验证范围。
3. **驱动 jar**：仅 drop-in 目录扫描，不做 Maven 坐标安装。
4. **DSN 方言语义**：`jdbc:<vendor>:...` 的 vendor 段即方言标识；契约层不上报
   per-connection 方言（A 档：vendor 仅识别不进 dialect 返回值）。

## 2. 目标 / 非目标 / 反目标

### 目标

- 新 cdylib 插件 `oj-db-jdbc`，实现既有 `DataAccessorVtable`（oj-plugin-ffi，vtable 形状
  不动，ABI 13 不变）。
- 认领 `jdbc:` DSN scheme；DSN = 完整 JDBC URL（或 `jdbc:{...}` JSON 信封，见 §5.2）。
- 覆盖 vtable 全槽位：connect / query / exec / begin / tx_* / dialect / schemes /
  stream_* ×4。
- Java 桥：单文件入口的 NDJSON 循环，驱动 jar 目录扫描加载。
- **宿主补丁仅一处**：`op_db_import` 增加 `require_known_dialect` 门控（见 §5.5，
  已核实为唯一漏网点）。除此之外宿主 core 零改动。
- e2e（env-gated）：达梦远程实例（主）、金仓（可选）、**H2（开箱即跑，任何人可复现）**。
- 随版本更新 CHANGELOG + docs/devkit 四件 + xtask 归置 + plugin-matrix CI 腿。

### 非目标（v1 明确不做）

- Maven 坐标解析 / 驱动下载（dbx 有独立 resolver 可执行文件，砍掉）。
- in-process JNI 嵌 JVM（与 deno/V8 同进程的信号、GC、崩溃域风险都不值得，对比结论见 §7）。
- `db.table(...)` SQL 构造器对 JDBC 方言的支持（占位符天花板，见 §5.5）。
- 复杂类型全映射（STRUCT/REF/自定义类型）：遇到转成可读字符串并在文档标注。
- 大 LOB 流式读：v1 只加上限报错（16MB），不做截断/流式。
- 请求级看门狗（FFI future 被 drop 时 cancel JDBC 语句）：v1 走 `socketTimeout`
  文档约束 + 32 线程硬顶说明（见 §7）。
- JDBCX 之类的高级驱动包装生态；sidecar 空闲回收。

### 反目标

- 宿主 core 为本插件加任何**特例分支**——唯一允许的宿主改动是 §5.5 的 db.import
  门控补漏（一行，通用逻辑非特例）。
- 从源码编译 V8 之外的第二个重型构建时依赖失控：Java 桥必须单模块、wrapper 提交
  （含审计纪律，见 §5.1）、CI 一条腿。

## 3. 总体形态

```
handler (JS) ──op──▶ bridge/host ──vtable──▶ oj-db-jdbc.so ──NDJSON(stdio)──▶ JVM (oj-jdbc-bridge.jar)
                                                    │                              │
                                              tokio runtime                  DriverManager + URLClassLoader
                                              SidecarMgr（插件内建）            per-conn FIFO 执行队列
```

- **一个 sidecar 进程**服务全部连接（多路复用，conn_id 区分），插件生命周期内常驻；
  惰性启动（首次 connect）。崩溃后下一次请求触发重启，在途请求全部以明确错误失败
  （不自动重放——事务状态不可恢复）；**sidecar 重启即该插件全部 handle 立即失效**
  （JVM 全新进程 conn 表为空，旧句柄以 `session_disposition=reconnect` 报错，绝不隐式
  落到新 JVM）。
- 插件结构完全镜像 `plugins/oj-db-mysql`：自建 tokio runtime、`spawn_ffi_future` /
  `catch_future` 包装、handle 注册表。

## 4. 新增/改动清单

| 位置 | 内容 | 性质 |
|---|---|---|
| `java/jdbc-bridge/` | 新建。Gradle 单模块（wrapper 提交），Java 11，`oj-jdbc-bridge.jar` | 新代码 |
| `plugins/oj-db-jdbc/` | 新建。cdylib crate，vtable + SidecarMgr + 协议编解码 | 新代码 |
| `src/bridge/db_import.rs`（或 op 位） | **一行门控补漏**：`op_db_import` 前置 `require_known_dialect` | 宿主小补丁 |
| `oj-plugin-ffi` / `serve` / `oj` 其余 | 零改动 | — |
| `tools/xtask` | 第一方插件清单加 `oj-db-jdbc`；归置 jar 到 `bin/plugins/<triple>/jdbc/`；`--check` 预检缺 jar 清晰报错 | 小改 |
| `.github/workflows/plugin-matrix.yml` | Java 腿（`actions/setup-java` + `cache: gradle`）；Windows 腿受 Rust 假 java 夹具保护 | 小改 |
| `docs/plugins/jdbc.md` | 新文档，硬性内容见下 | 新文档 |
| `docs/devkit/` 四件 + `CHANGELOG.md` | 按红线同步（api-manual 限制表加 jdbc 行） | 例行 |
| `sample/` | H2 开箱样例（任何 clone 者可跑）+ env-gated 达梦样例 | 小改 |

`docs/plugins/jdbc.md` **必须**含（产品评审 S1/S2/M1-M5 硬性化）：①目标机准备
checklist（`java -version` 验证、发行布局 jar 放置路径图、各厂商 jar 获取指引表、
私有化装 JRE 指引）；②「从拿到客户环境到第一个查询」全程步骤 ≤10 分钟；③ENC 密封
范式（整串 seal）与信封内嵌 ENC 不可用的醒目框；④错误文案对照排障表；⑤限制节
（SQL 业务全责、单厂商/部署及多实例配方、LOB/大结果上限、`''`→NULL、UTC 时区口径、
DDL 隐式提交、drop-in 需重启及原因、.so 与 jar 锁死升级须知）；⑥db.export 是受支持
路径的正面说明。SKILL.md 陷阱表 v1 预置行：`dialect()` 恒 `"jdbc"`；构造器/listTables/
migrate/seed/import 在 jdbc 后端 unsupported 不是 bug、改用 `db.query`+`?`；DECIMAL
带 scale 回 string、scale=0 走整数；`''` 读回 null；信封内嵌 `ENC[...]` 不解密。

## 5. 关键设计

### 5.1 sidecar 进程管理（Rust，插件内）

- `SidecarMgr`：持有 `tokio::process::Child` + stdin 写任务（unbounded mpsc 消费）+
  stdout 读任务 + **stderr 常驻 drain 任务** + pending 请求表（req_id → oneshot）。
  NDJSON 帧 = 一行一个 JSON 对象，**未知键一律忽略**（协议加字段可不走 proto bump）。
- **stderr drain 是硬性要求**（不是可选日志）：JVM/驱动 stderr 写满管道缓冲（~64KB）
  即阻塞整个 sidecar → Rust 侧全部 pending 挂死且 child 活着不触发重启路径（楔形死锁）。
  drain 任务逐行转发 HostContext log。单测覆盖「stderr 喷 >64KB 时请求仍正常」。
- **行长上限 64 MiB**：超过即判协议损坏 → sidecar 置 dead 走重启路径（防 native 库直写
  fd 1 的无换行垃圾永远楔死 reader）。
- **req_id 由 Rust 侧全局单调分配（u64），sidecar 重启绝不归零**；Java 只回显不分配。
  spawn 后先同步读 hello 行再放行请求；hello 不匹配报两侧版本号。
- 启动命令：
  `java -Duser.timezone=UTC -Dfile.encoding=UTF-8 [-Xmx512m] -cp <bridge.jar><sep><drivers_dir>/* app.oj.jdbc.Bridge`
  （`<sep>` = `if cfg!(windows) ";" else ":"`）。
  `-Duser.timezone=UTC` 是时区口径契约的一部分（见 §5.4），缺省必带；
  `-Xmx512m` 缺省值与「大结果必须走 stream_*」一起写进文档。
- **jar 发现**：`<本 cdylib 所在目录>/jdbc/oj-jdbc-bridge.jar`。插件自身路径用
  `libc::dladdr` 取**导出符号**（`oj_plugin_abi_version`，宿主 dlsym 它故必被导出；
  勿取 crate 私有函数，LTO 可能局部化）的基址解析（unix；Windows 无 dladdr →
  `java_bridge` cfg 必填）。cfg/env 可覆盖。classpath 单参数 Windows 有 ~32KB 命令行
  上限，jar 极多时升级路径是 Java 9+ `@argfile`（代码注释标注，v1 不做）。
- `java` 解析顺序：插件 cfg `java_bin` → `JAVA_HOME/bin/java` → `PATH`。三者皆无 →
  connect 返回结构化错误（文案含搜索顺序 + 三条修复路径）。
- stdout 只走协议：Java 侧 `System.out` 重定向到 stderr（防驱动杂讯污染协议流；
  注意这挡不住 native 库直写 fd 1——由行长上限兜底）。
- **connect 失败必须附 stderr 尾部**（最后 ~20 行）：java 缺失/驱动加载失败/classpath
  错误时 child 秒退，无 stderr 尾则部署机上不可诊断。
- **EOF 退出契约（硬性）**：Java 桥必须实现「stdin EOF（readLine 返回 null）→ 有序
  关闭全部 Connection 并退出」。协议层写明 + 单测覆盖干净退出路径。限制：宿主
  SIGKILL/crash 时 JVM 成孤儿、继续持有 DB 连接至 JDBC socket 超时——文档化，并以
  连接属性 `socketTimeout` 缺省注入收窄（见 §7）。
- pending 表必须容忍 receiver 已 drop（宿主 KillSwitch 丢弃 JS runtime 时 `await_ffi`
  被 drop，sidecar 响应回来时 send 失败静默丢弃即可）。

### 5.2 DSN 与配置

- `schemes()` = `["jdbc:"]`。装配层按 `dsn.starts_with(scheme)` 认领路由
  （db_backend.rs，已核实），**所有形态都必须以 `jdbc:` 开头**。
- connect 收到的 DSN 两种形态：
  1. 裸 JDBC URL：`jdbc:oracle:thin:@//127.0.0.1:1521/FREEPDB1`（凭据须在 URL 内）。
  2. **带 scheme 前缀的 JSON 信封**：`jdbc:{"url":"jdbc:dm://…","user":"u","password":"p","props":{"compatibleMode":"mysql"}}`
     —— 插件剥掉 `jdbc:` 前缀后见 `{` 即按 JSON 解析。企业驱动普遍需要额外连接属性，
     这是唯一入口。`password` 支持 `{"$env":"DB_PASS"}` 环境变量引用。
- **ENC 密封范式（与宿主配置红线对齐，已核实 secret.rs）**：config.yaml 里**静态声明**
  的 DSN（裸 URL 或整条信封）一律 `oj secret seal` **封整条字符串**——宿主配置装载期
  `decrypt_tree` 递归解密，早于 connect，零插件支持。**信封内嵌 `ENC[...]` 字段不解密**
  （`is_sealed` 要求整串密封），文档醒目框 + SKILL 陷阱表明写。`$env` 降级为 JS 运行时
  `db.client.register` 场景（无 config 静态位可封时）。
- 插件 cfg（`plugins: oj-db-jdbc:` 段透传）：
  ```yaml
  plugins:
    oj-db-jdbc:
      drivers_dir: /abs/path/jdbc/drivers   # 缺省 <插件目录>/jdbc/drivers（dladdr）
      java_bin: /path/to/java               # 可选
      java_opts: ["-Xmx512m"]               # 可选；UTC/file.encoding 由插件强制前置
  ```
  信封 `props` 增可选 **`driver_class`**（如 `dm.jdbc.driver.DmDriver`）：
  老版本商用驱动 jar 可能缺 `META-INF/services/java.sql.Driver` 条目，显式
  `Class.forName(name, true, ucl)` 作为逃生舱（见 §5.3）。

### 5.3 Java 桥协议（NDJSON）

请求：`{"id":1,"op":"connect","url":"...","user":"...","password":"...","props":{}}`
响应：`{"id":1,"ok":{...}}` 或 `{"id":1,"err":"..."}`，行尾 `\n`，UTF-8。未知键忽略。

op 与 vtable 一一映射：`connect → {"handle":1}`；`query → 行数组`；`exec → 受影响行数`；
`begin → {"tx_id":1}`；`tx_query/tx_exec/tx_commit/tx_rollback`；
`stream_open → {"stream_id":1}`；`stream_next → {"rows":[...]}` / `{"done":true}`；
`stream_cancel/stream_close`；`disconnect`（close 槽位触发，见下）。

- **per-conn FIFO 串行化（硬性）**：`java.sql.Connection` 非线程安全。Java 侧每 conn_id
  一条单线程执行队列（等价独占锁），同 conn 的 op 严格有序——`tx_query` 与 `query`
  并发交错会损坏驱动协议流（表现为 hang/错包）。**`Statement.cancel()` 是唯一允许
  绕过队列的调用**（JDBC 规范明确允许跨线程）。
- **close 槽位（同步签名）严禁任何阻塞写**：`disconnect` 经 unbounded mpsc **非阻塞**
  投递给 stdin 写任务，落 per-conn 队列尾（天然排在在途 op 之后）。close 不从 pending
  表抢在途请求（它们照常完成，语义与 mysql 插件一致）。Java 侧对未知/已关闭 conn_id
  的 op 回结构化错误，绝不 NPE。
- 阻塞 JDBC 调用跑在 cached 线程池（上限 32，仅 cancel 独立线程不占池）；实际并发被
  per-conn 队列吞吐限制，32 只是兜底上限（慢查询钉死问题见 §7）。
- **cancel 响应唯一性**：`Statement.cancel` 由独立线程执行后，执行线程的 JDBC 调用
  也会以 `SQLException` 返回——两条路径抢同一 id 的响应帧。每请求一个 claimed 标志，
  先赢者写帧，败者丢弃。
- **驱动加载**：建 URLClassLoader 装载 drivers 目录全部 jar 后
  `Thread.currentThread().setContextClassLoader(ucl)`（ServiceLoader 读 TCCL）；TCCL 设置
  **不足以**让 DriverManager 选用 UCL 驱动——`DriverManager.getConnection` 的
  `isDriverAllowed` 要求调用方类加载器可见驱动类，必须再用 **DriverShim**（app 类加载器
  的 `Driver` 实现，包装 UCL 加载的驱动实例）`registerDriver` 后走 DriverManager
  （T2 评审实测复现：仅 TCCL 时 connect 恒报 "No suitable driver"）；信封给了
  `driver_class` 则显式 Class.forName 作为缺服务条目 jar 的逃生舱。
- `SQLFeatureNotSupportedException` 必须翻译成结构化「unsupported」帧（含能力名与
  驱动版本），不得落成 generic err——否则 stream/cancel 在驱动缺实现时定位不到原因。

### 5.4 类型映射（双向，按 Oracle/达梦/金仓驱动实测语义修正）

JSON → JDBC（`params` 数组按位绑定 PreparedStatement）：

| JSON | JDBC |
|---|---|
| `$oj$i64` 标记（jsint） | `setLong`（`marker_i64` 先行识别） |
| `$oj$u64` 标记（jsint） | `setBigDecimal`（JDBC 无 u64，Decimal 全方言可承载） |
| null | **`setNull(i, Types.VARCHAR)`**——Oracle/达梦对 `Types.NULL` 报 "Invalid column type"；VARCHAR 空绑定做隐式转换覆盖绝大多数场景（PG/MySQL/金仓均接受）。不依赖 ParameterMetaData（Oracle/DM 实现参差且多一次往返） |
| 整数 | `setLong` |
| 浮点 | `setDouble` |
| 字符串 | `setString`；**超阈值（如 4000 字符）切 `setClob/setCharacterStream`**（Oracle/DM 超长隐式转 CLOB 报 ORA-01461 类错误） |
| 布尔 | `setBoolean` |
| 对象/数组 | `setString(i, json)`（文档标注：复杂参数先序列化） |
| `"$blob:<base64>"`（cellenv 信封） | `cellenv::decode` 后 `setBytes`；超阈值切 `setBinaryStream` |

JDBC → JSON（ResultSetMetaData.getColumnType + getScale 驱动）：

| JDBC 类型 | JSON |
|---|---|
| NULL | null |
| BOOLEAN/BIT | bool |
| 整数族 | **统一 `getBigDecimal` 判幅值**（直接 getLong 遇超 i64::MAX 抛异常）：≤2^53-1 → number；≤i64::MAX → `$oj$i64`；更大 → `$oj$u64`（与 mysql 插件 `BIGINT UNSIGNED` 行为对齐） |
| **DECIMAL/NUMERIC** | **先判 `getScale(i)`：scale==0 → 走整数族分支**（DM8 驱动把 INT/NUMBER(p,0) 上报为 NUMERIC——不判 scale 达梦整数列全变 string）；scale>0 → **string**（保精度防 f64 丢位） |
| FLOAT/DOUBLE/REAL | number |
| DATE/TIMESTAMP | ISO-8601 string（`getTimestamp`，JVM 固定 UTC，见 §5.1；标准 DATE 列得 00:00:00 属表示层扩展，可接受） |
| **TIME** | **单独 `getTime` → `"HH:MM:SS"` string**——JDBC 规范对 TIME 调 getTimestamp 未定义（epoch 日期或抛错，驱动各自为政） |
| **TIMESTAMP_WITH_TIMEZONE** | `getObject(i, OffsetDateTime.class)`（JDBC 4.2；DM8/金仓/PG 驱动均支持）——getTimestamp 会丢 offset 造信息 |
| CHAR/VARCHAR/LONGVARCHAR/CLOB/NCHAR… | string（**CLOB 超 16MB 上限报结构化错误**，v1 不做流式） |
| BLOB/BINARY/VARBINARY/LONGVARBINARY | `cellenv::encode("blob", bytes)` → `"$blob:<base64>"`（复用既有约定，宿主 degraded 清单兼容）；**超 16MB 上限报结构化错误** |
| ARRAY | JSON 数组（元素递归） |
| STRUCT/OTHER/REF/ROWID | string（`getString`，文档标注为可读降级） |

已知厂商语义陷阱（文档/SKILL 陷阱表 + e2e 规避）：Oracle/达梦把 `''` 存成 NULL
（insert '' select 回 null）；Oracle/DM 事务内 DDL 隐式提交（rollback 不回滚 DDL，
e2e 不得把 DDL 写进事务链）；DM 默认 CASE_SENSITIVE=1（e2e 统一不引用标识符/大写，
首连探测参数值，异常时 skip 原因打印）。

### 5.5 方言与构造器

- **DSN 方言语义**：`jdbc:<vendor>:...` 的 vendor 段即方言标识（用户已定）。插件内置
  厂商识别表（仅识别，不改变运行时行为）：
  `postgresql`→postgres 族、`mysql`→mysql 族、`oracle`→oracle、`dm`→dameng、
  **`kingbase8`**→kingbase（金仓真实子协议是 `kingbase8`，com.kingbase8.Driver；
  PG 兼容模式也用 kingbase8 驱动而非 jdbc:postgresql://）、`sqlserver`/`microsoft`→mssql、
  其余→原样记日志。识别结果用于诊断日志、错误文案与文档。
- **`dialect(handle)` 恒返回 `"jdbc"`**（宿主 T5 一致性校验要求逐字等于工厂级
  `describe().dialect`，ffi.rs:380 已核实）；`describe()` 报：
  `{"dialect":"jdbc","capabilities":["placeholder_q","id_two_step","stream","cancel"]}`。
  - `id_two_step` 而非 `id_returning`：id 组强制恰好一枚（caps_from_json 已核实），
    不能撤只能选；且 getGeneratedKeys 各厂支持参差（Oracle 历来不可靠），未逐厂验证
    前不声明 returning。该值在 `"jdbc"` 门控下无消费者，注释说明即可。
- **占位符天花板（构造器不可用的根因，已核实）**：构造器路径 `build_sql`
  （query.rs:2834）用 sea-query `PostgresQueryBuilder` 原生烘焙 `$n` 占位符，而 JDBC
  PreparedStatement 任何厂商只认 `?`；宿主 `normalize_sql` 仅 `?`→`$n` 单向改写。
  即使上报 "postgres" 方言名，构造器 SQL 也绑不上。
- **宿主门控现状（全部核实）**：生成 SQL 路径中构造器/list_tables/next_seq/
  migrate/seed 均过 `require_known_dialect`，`"jdbc"` 不在 known 子集 → 结构化
  `unsupported`，不回落 sqlite（db.rs:62）。**唯一漏网：`db.import`**——
  `db_import.rs:1964` 无门控调 `da.dialect()`，而 `FfiDataAccessor::dialect()`
  （ffi.rs:428）对未知方言 **panic**（fail-loud 设计），panic 发生在 detached
  `tokio::spawn` 的 import 任务内 → 任务卡死 Running 态 + stderr panic，而非结构化
  错误。**宿主补丁（唯一一处）**：`op_db_import` 前置一行
  `require_known_dialect` 门控，行为与其余路径对齐。与其配套的 import e2e 断言
  jdbc 后端报 structured unsupported。
- **受支持路径的正面清单**：裸 SQL `db.query`/`db.exec`（JS 写 `?`，placeholder_q
  直通）；**`db.export` 实际可用**（走 `dialect_info().caps.placeholder`，
  db_export.rs:558 已核实，无 known-dialect 门控）——文档列为受支持路径并加一条 e2e；
  `db.stream` 系列（经 stream_* 槽位）。JS 侧 `db.dialect()` 一律报 `"jdbc"`。

### 5.6 事务与流式

- 每 handle = 一条 JDBC Connection。`begin` 关 autoCommit，`tx_*` 在该连接上执行，
  commit/rollback 后恢复。与 mysql 插件语义一致。DDL 隐式提交陷阱见 §5.4。
- 流式：`stream_open` 创建 Statement（`setFetchSize(500)`，shortcut: 硬编码，出现
  具体驱动问题时再外提），`stream_next` 批量 100 行，`stream_cancel` 置标志 +
  `Statement.cancel()`（唯一允许跨线程的 JDBC 调用），`stream_close` 释放。
- **驱动能力探活**：DM8/金仓驱动宣称支持 setFetchSize/Statement.cancel 但实现质量
  参差——主验证腿首连先跑探针用例（开流、拉一批、cancel），通过后再固化 golden
  fixture；`SQLFeatureNotSupportedException` 已按 §5.3 译成结构化 unsupported。

### 5.7 驱动 jar 目录

- 缺省 `<oj-db-jdbc cdylib 所在目录>/jdbc/drivers/*.jar`（dladdr 解析；cfg
  `drivers_dir` / env `OJ_JDBC_DRIVERS_DIR` 可覆盖）。
- Java 侧启动时 `URLClassLoader` 加载该目录全部 jar（TCCL 设置见 §5.3），
  DriverManager 按 URL 自动选型。drop-in 新 jar 需重启 oj 进程（驱动类静态状态 +
  URLClassLoader 不可卸载）——文档写明原因，减少「应支持热加载」的误判。
- **达梦 jar 许可**：Dm8JdbcDriver18 属达梦商用许可，仓库/CI/docs **一律不得携带**
  jar——e2e 只能外部注入；文档给出获取指引（DM 客户端安装目录 `drivers/jdbc/`），
  许可自负。金仓/Oracle jar 同理（官方下载入口）。

### 5.8 错误映射（结构化信封）

- JDBC `SQLException` → 宿主 `{category,retryable,session_disposition}` 信封：镜像
  oj-db-mysql 的 SQLState class 表（08xxx→connection/reconnect、40xxx→retryable 等），
  归类逻辑单侧放 Rust（Java 桥原样回传 `sqlState`/`errorCode`/`message`）。
  **SQLState 非标准兜底**：达梦/金仓部分错误给 null 或厂商码——拿不到标准 SQLState
  时按 `errorCode` 段粗分类或直接 internal，**不硬套 MySQL 表后静默误判 retryable**。
- **错误消息凭据脱敏（硬性）**：JDBC 驱动 connect 失败消息普遍回显完整 JDBC URL
  （含 `user:password@`），插件经 `parse_raw` 的信封原样进 JS → 可落 HTTP 响应与
  logs/。插件 Rust 侧构造错误前统一脱敏（`://[^@/\s]*@` → `://***@`，镜像宿主
  `secret::redact` 语义）；Java 桥 stderr 日志同理（多租户共享日志槽位）。
- **sidecar 崩溃**：该 sidecar 名下全部 handle/stream 立即失效（reconnect），在途
  pending 全部投递明确错误；事务随 JVM 消亡回滚（语义安全）；下一次请求触发重启。
  绝不自动重放。
- **错误文案规范**（目标文案进文档）：缺 java → 含搜索顺序（cfg → JAVA_HOME →
  PATH）+ 三条修复路径；驱动目录为空 → 打印**实际扫描的绝对路径**（放错目录是唯一
  自救信息）；proto 不匹配 → 打印两侧版本号 + 「.so 与 jar 必须同版整目录拷贝」。

### 5.9 多厂商驱动共存（v1 文档化限制）

单 JVM、单 URLClassLoader 装载 drivers 目录全部 jar。DM + Kingbase + Oracle 驱动
并存时的类冲突/静态状态串扰是 JDBC 生态老问题，v1 **接受「每个部署只放一家驱动」**。
文档正面表述 + 配方：「v1 单部署单厂商；多厂商按厂商起多个 oj 进程/容器实例」
（不同 `plugins.oj-db-jdbc.drivers_dir` + 不同端口）。若未来出现真实多厂商同机需求，
sidecar 按 vendor 分段进程（SidecarMgr 从单例变 keyed）。

## 6. 测试计划（用例驱动，env-gated 腿诚实登记）

1. **Rust 单元**（`cargo test --release -p oj-db-jdbc`）：
   - NDJSON 帧编解码（分片/粘包/非法行/非 UTF8/**超长无换行行**/未知键忽略）。
   - DSN 解析（裸 URL / `jdbc:{...}` 信封 / 信封缺 url / `{"$env"}` 展开 /
     **整串 ENC 密文透传**——解密发生在宿主，插件原样收明文）。
   - vendor 解析表（含 `kingbase8`）。
   - SidecarMgr：**假 java 用 Rust 辅助 bin 夹具**（tests fixture，随包构建，回显
     固定 NDJSON 行）——三平台可跑，Windows 腿不裸奔（plugin-matrix 有
     windows-latest，已核实）；覆盖 connect/query/**stderr 喷 >64KB 请求仍正常**/
     **干净 EOF 退出**/**崩溃重启后旧 handle 立即失效**/close 与在途请求竞态。
   - 错误脱敏断言（含 `user:pass@` 消息 → `://***@`）。
2. **Java 单元**（`java/jdbc-bridge`，JUnit + H2 test-only）：每 op 语义；类型映射
   golden fixture（**含 setNull(VARCHAR)、scale==0 整数分流、TIME 单列、TZ 列**
   用例）；cancel 竞态（响应唯一性）；per-conn 串行化；TCCL/driver_class 加载；
   OOME 兜底回错误帧；**EOF 有序退出**。
3. **e2e**（`plugins/oj-db-jdbc/tests/`，env-gated，凭据 env 注入、不进仓库）：
   - `OJ_TEST_JDBC_DM_URL` / `_USER` / `_PASSWORD`：达梦远程实例（开发期提供，如
     `jdbc:dm://192.168.67.64:5237`）；缺 env skip。**前置探活**：fetchSize/cancel
     支持探针 + CASE_SENSITIVE 探测（异常 skip 原因打印）。用例含 **insert null
     列断言**、整数列 number 断言（验证 scale 分流）、空串剔除或断言 null、
     时间值 UTC 口径、stream 批边界、cancel、db.export、import 门控 structured
     unsupported。
   - `OJ_TEST_JDBC_KINGBASE_URL` 等：金仓可选腿（`jdbc:kingbase8://...`；官网
     Docker tar + 试用 license，就绪前恒 skip）。
   - **H2 开箱腿（默认可跑）**：H2 jar（Maven 中央，Apache-2.0）随测试夹具提供，
     `jdbc:h2:mem:` 走完整链路——任何人 clone 后可复现「丢 jar 即接」全程。
   - Oracle 不在 v1 验证范围。
4. **宿主补丁测试**：op_db_import 门控后 jdbc 后端报 structured unsupported（不
   panic、不卡死）。
5. **契约一致性**：`cargo xtask plugin oj-db-jdbc --check` + devkit 校验用例。

## 7. 风险与已知限制（必须随文档公开）

- 首 connect 承担 JVM 冷启动（**e2e 记录实测数字进文档**，部署手册给预热配方：
  启动后跑一次健康检查查询）。
- 单 sidecar 是故障域：其崩溃在途请求全失败（事务必然回滚，语义安全）；**JVM OOME
  由 Java 侧兜底成错误帧**，避免半死状态。
- **慢查询钉死**：非 stream 的 db.query/exec 无取消（看门狗只杀 JS 侧，JVM 里阻塞
  语句继续占池）。cached 池 32 是长查询并发硬顶——文档强制连接属性 `socketTimeout`
  并写明后果；请求级看门狗（FFI drop → cancel）是 v2 候选。
- 多租户共享 JVM：连接级无跨租户泄漏（错误消息随请求走），但驱动 stderr 日志可能
  含他租户 SQL/连接串，落共享日志槽位——文档说明。
- 宿主 SIGKILL/crash → 孤儿 JVM 持有 DB 连接至 socket 超时——文档化 +
  `socketTimeout` 缺省注入收窄。
- 大结果全量缓冲（v1 无流式导出上限之外的保护）：`-Xmx512m` 缺省 + 「大结果必须走
  stream_*」文档指引；LOB 单列 16MB 上限结构化报错。
- DECIMAL(scale>0)→string、`''`→null、UTC 时区口径——均进 SKILL 陷阱表。
- 目标机必须有 Java 11+；`oj` 的"拷贝即跑"承诺对本插件附加此前提（JRE + 厂商 jar）。
- 为什么不是 JNI in-process：崩溃域（JDBC 驱动 segfault 杀整个 oj 进程）、V8 ×
  HotSpot 信号处理器共存（SIGSEGV/SIGBUS 双 handler 链式冲突）、JVM per-process
  单例 vs 多插件模型、部署耦合（libjvm 版本匹配）——sidecar 全维度胜出，仅边际
  延迟劣势（瓶颈本在 JDBC 网络 IO）。

## 8. 实施顺序（建议）

1. 宿主一行补丁：`op_db_import` 门控 + 测试（先行，独立可合并）。
2. `java/jdbc-bridge` 骨架 + NDJSON 循环 + per-conn 队列 + EOF 退出 + JUnit/H2。
3. `plugins/oj-db-jdbc` vtable 全槽位 + Rust 假 java 夹具单测（跨平台）。
4. 两侧对接 golden fixture；H2 开箱冒烟（dev 手工）。
5. xtask 归置（含 jar）+ CI 腿（setup-java + gradle 缓存 + wrapper 审计：
   distributionUrl 钉版本、wrapper jar 校验和入库）。
6. env-gated e2e：达梦远程（主，前置探活）；金仓（就绪时）。
7. 文档：jdbc.md（含 §4 全部硬性内容）+ SKILL 陷阱表 + devkit 四件 + CHANGELOG +
   sample（H2 开箱 + 达梦 env-gated）。
