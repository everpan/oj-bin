# oj-db-jdbc

## 概述

泛型 JDBC 数据库插件：`jdbc:` DSN 接达梦 / 金仓 / Oracle / H2 等**任何有 JDBC 驱动**
的企业库。宿主（Rust cdylib）不内嵌 JVM——采用 **sidecar 架构**：插件首次请求时
惰性 spawn 一个 Java 子进程（bridge jar + 驱动目录），stdin/stdout 走 NDJSON 帧
协议（一行一个 JSON 对象），stderr 常驻 drain 进宿主日志。驱动崩溃/segfault 只死
sidecar，杀不到 oj 进程——这是不选 JNI in-process 的核心原因（崩溃域 + V8 ×
HotSpot 信号处理器冲突 + libjvm 版本耦合，sidecar 全维度胜出，代价仅是边际延迟，
瓶颈本在 JDBC 网络 IO）。

设计细节见 `docs/superpowers/specs/2026-11-20-jdbc-generic-plugin-design.md`；
Java 桥源码与协议见 `java/jdbc-bridge/README.md`。

## 提供的后端轴

`db`（DSN 以 `jdbc:` 开头即由本插件认领）。

## 目标机准备 checklist

- [ ] **`java -version` 可执行且 ≥ 11**（Java 11+；私有化部署装一个 JRE 即可，
      如 Eclipse Temurin——sidecar 只跑桥 + 驱动，不需要 JDK 编译器）。
      解析顺序：`plugins.oj-db-jdbc.java_bin` → `JAVA_HOME/bin/java` → `PATH`，
      三者皆无 → connect 报结构化错误（文案含搜索顺序与三条修复路径）。
- [ ] **发行布局就位**（`cargo xtask build` / `cargo xtask plugin db-jdbc`
      统一归置）：

      ```
      bin/
        oj
        plugins/<host-triple>/
          oj-db-jdbc.so              # 插件本体
          jdbc/
            oj-jdbc-bridge.jar       # xtask 归置（缺它 --check 预检直接报错）
            drivers/                 # ← 你在这里投放厂商驱动 jar（*.jar 全量装载）
              DmJdbcDriver18.jar
      ```

- [ ] **厂商驱动 jar 已放入 `jdbc/drivers/`**。jar 不入 `-cp`、不进仓库
      （许可自负，见下表）；本插件目录内已有 `drivers/README` 占位说明。

### 厂商 jar 获取指引

> ⚠️ **许可自负**：下表 jar 均为各厂商商用/官方许可，仓库、CI、发行文档一律不携带
> jar 本体。获取与使用遵守你与厂商的许可协议；生产分发时驱动 jar 由部署方自备。

| 数据库 | 推荐驱动 | 获取入口 |
|---|---|---|
| 达梦 DM8 | `com.dameng:Dm8JdbcDriver18`（`dm.jdbc.driver.DmDriver`，`jdbc:dm://host:5237`） | DM 客户端安装目录 `drivers/jdbc/`，或达梦官方 Maven 镜像 |
| 金仓 Kingbase | `com.kingbase8`（`com.kingbase8.Driver`，**子协议是 `kingbase8` 不是 `postgresql`**） | 官网下载 / 官方 Docker 镜像内提取；PG 兼容模式也用 kingbase8 驱动 |
| Oracle | `ojdbc11`（`jdbc:oracle:thin:@//host:1521/SID`） | Oracle 官网 JDBC 下载页 |
| H2（开发/开箱验证） | `com.h2database:h2:2.3.232`（**Apache-2.0，可自由携带**） | Maven 中央：`https://repo1.maven.org/maven2/com/h2database/h2/2.3.232/h2-2.3.232.jar` |
| MySQL / PG（桥接形态） | 官方 `mysql-connector-j` / `postgresql` | Maven 中央——但这两家优先用第一方插件 `oj-db-mysql` / `oj-db-postgres`，只有需要 JDBC 专有属性时才走本插件 |

缺 `META-INF/services/java.sql.Driver` 条目的老旧商用 jar：信封 `props` 加
`"driver_class": "dm.jdbc.driver.DmDriver"` 显式装载（逃生舱，见下）。

## 「从拿到环境到第一个查询」全程（H2 开箱腿，≤10 分钟）

H2 是 Apache-2.0 的内存库，任何 clone 本仓库的人可全程复现：

```bash
# 1. 构建插件 + 归置桥 jar（备好 jdbc/ 与 jdbc/drivers/ 空投目录）
cargo xtask plugin db-jdbc

# 2. 拿 H2 驱动 jar（Maven 中央）丢进 drivers 目录
curl -L -o bin/plugins/<host-triple>/jdbc/drivers/h2-2.3.232.jar \
  https://repo1.maven.org/maven2/com/h2database/h2/2.3.232/h2-2.3.232.jar
#   <host-triple> 以实际发行布局为准（如 aarch64-apple-darwin），
#   即 <exe>/plugins/<host-triple>/jdbc/drivers/

# 3. config.yaml 配 DSN（H2 内存库 + 常驻属性）
#    db:
#      default: 'jdbc:{"url":"jdbc:h2:mem:demo","user":"sa","props":{"DB_CLOSE_DELAY":"-1"}}'

# 4. 起服务后打第一个查询（JVM 由首次 connect 惰性拉起，这一步付冷启动账）
./bin/oj exec -c config.yaml -e \
  'const rows = await db.query("select 1 as one", []); console.log(JSON.stringify(rows));'
# → [[{"one":1}]]（H2 把 INT 上报 NUMERIC，scale==0 → 整数族分流回 JSON number）
```

达梦真实库的最小配置见 [ENC 密封范式](#enc-密封范式)与
[`sample/config.yaml`](../../sample/config.yaml) 的 `db.dm` 注释 + 可运行样例
[`sample/src/dm/query/api.ts`](../../sample/src/dm/query/api.ts)。

## 配置

### DSN 两种形态（都必须以 `jdbc:` 开头）

```yaml
db:
  # 形态 1：裸 JDBC URL（凭据须在 URL 内）
  legacy: 'jdbc:oracle:thin:@//127.0.0.1:1521/FREEPDB1'
  # 形态 2：JSON 信封（企业驱动普遍要额外连接属性，这是唯一入口）
  dm: 'jdbc:{"url":"jdbc:dm://127.0.0.1:5237","user":"SYSDBA","password":"<your-password>","props":{"compatibleMode":"mysql","socketTimeout":"30000"}}'
```

- 信封 `props` 全量上送 JDBC（`socketTimeout` 建议显式给，见「限制」）。
- 信封 `password` 支持 `{"$env":"DB_PASS"}` 环境变量引用——这是**运行时**
  `db.client.register` 场景（config 无静态位可封时）的降级手段；config 静态
  声明一律整串 ENC（见下框）。
- 插件段透传（`plugins:` 段）：

  ```yaml
  plugins:
    oj-db-jdbc:
      drivers_dir: /abs/path/jdbc/drivers   # 缺省 drivers/（相对 oj 进程 CWD）——建议恒显式绝对路径
      java_bin: /path/to/java               # 可选，最高优先
      java_opts: ["-Xmx512m"]               # 可选；UTC 与 file.encoding 由插件强制前置
  ```

### ENC 密封范式

> **凭据只走整串 ENC 密封。** config.yaml 里静态声明的 DSN（裸 URL 或整条信封）
> 一律 `oj secret seal` **封整条字符串**——宿主配置装载期 `decrypt_tree` 递归解密，
> 早于 connect，零插件支持：
>
> ```bash
> echo -n 'jdbc:{"url":"jdbc:dm://127.0.0.1:5237","user":"SYSDBA","password":"<your-password>"}' \
>   | ./bin/oj secret seal -k keys/secrets-public.pem
> # config.yaml:
> #   dm: 'ENC[...]'
> ```
>
> **信封内嵌 `ENC[...]` 字段不解密**（`is_sealed` 要求整串密封）——
> `jdbc:{"url":"...","password":"ENC[...]"}` 会把密文**当明文密码**发给驱动。
> 有 `ENC[...]` 却无私钥 → 启动 fail-fast，绝不静默当明文用；私钥只放部署机
> （`OJ_SECRET_KEY` / `OJ_SECRET_KEY_FILE` / `secrets.private_key_path`），不得进仓库。

## 方言语义与生成 SQL 门控

- JS 侧 `db.dialect()` **恒返回 `"jdbc"`**；`plugins()` / `ojInfo()` 的能力串：
  `{"dialect":"jdbc","capabilities":["placeholder_q","id_two_step","stream","cancel"]}`。
- `jdbc:dm:` / `jdbc:oracle:` 等的 vendor 段只进诊断日志与错误文案，
  **不改变运行时行为**（内置识别表：postgresql→postgres 族、mysql→mysql 族、
  oracle、dm→dameng、**kingbase8**→kingbase、sqlserver/microsoft→mssql）。
- **生成 SQL 路径被结构化门控**：构造器（`db.table(...)`）/ `listTables` /
  `nextSeq` / `oj migrate` / seed 重放 / `db.import` 在 `"jdbc"` 方言上一律报
  `OJ_DB_UNSUPPORTED`（`e.data.capability` 点名 `query_build` / `list_tables` /
  `next_seq` / `migrate` / `seed` / `import`）——**不是 bug**。根因：构造器烘焙
  的占位符是 `$n`，JDBC PreparedStatement 任何厂商只认 `?`，绑不上。
- **受支持路径的正面清单**（jdbc 后端上照常可用）：
  - `DB("dm").query(sql, params)` / `.exec`——裸 SQL + `?` 绑参（`placeholder_q`
    直通），这是你 95% 的写法；
  - `db.stream` 系列（真流式，`stream`/`cancel` 能力位；见下「限制」cancel 一节）；
  - **`db.export` 任务式导出照常可用**——它走流式泵（`dialect_info().caps.placeholder`
    + stream 槽位），不受 known-dialect 门控；大数据量出仓**无需**绕行。

### 达梦实测边界（DM8 / Dm8JdbcDriver18 8.1.1.49 实测，e2e 钉死）

| 类别 | 事实 |
|---|---|
| 支持 | `LIMIT`/`OFFSET`、标准 `OFFSET FETCH`、`TOP n`、MERGE INTO、序列（`NEXTVAL`/`CURRVAL`）、`IDENTITY` 列、`@@IDENTITY`、`TRUNCATE`、`?` 占位符 |
| **不支持** | `INSERT IGNORE`、`ON DUPLICATE KEY UPDATE`、`INSERT ... RETURNING`——幂等写请用 MERGE INTO 或先查后写（在 `db.tx` 内） |
| 空串 | `''` **原样保留**（不吞成 NULL）——与 Oracle 系传言相反，按实测钉死；但跨厂商代码别依赖任何一种，把它当驱动特定行为 |
| 整数列 | INT 被驱动上报为 NUMERIC——插件按 `getScale()==0` 分流回 JSON number；scale>0 的 DECIMAL/NUMERIC 一律回 **string**（保精度） |
| 标识符 | 默认 `CASE_SENSITIVE=1`：引号小写 `"name"` 与未引 `NAME` 是**不同对象**；裸 SQL 里统一不引用标识符或统一大写，别混用 |
| 多行绑参 | `insert ... values (?,?), (?,?)` 的**组数必须与参数个数同构**，否则 DM 报「序列号无效」（协议态错乱式报错，不是参数溢出） |

## 类型映射速览（双向）

写（JSON → JDBC）：整数 `setLong`；`|v|>2^53-1` 的整数走 `$oj$i64`/`$oj$u64`
标记（与 mysql 插件 BIGINT 行为对齐）；null → `setNull(Types.VARCHAR)`；浮点
`setDouble`；字符串（超 ~4000 字符自动切 CLOB 流）；布尔 `setBoolean`；
对象/数组 `setString(json)`（复杂参数先序列化）；`"$blob:<base64>"` → 字节流。

读（JDBC → JSON）：整数族统一 `getBigDecimal` 判幅值（防超 i64 抛错）；
DECIMAL/NUMERIC **先判 scale**（见上）；FLOAT/DOUBLE → number；DATE/TIMESTAMP →
ISO-8601 串（JVM 固定 UTC，见下）；TIME 单列 → `"HH:MM:SS"` 串；
`TIMESTAMP_WITH_TIMEZONE` → 带 offset 的 ISO 串；CHAR/VARCHAR/CLOB → string；
BLOB/BINARY → `"$blob:<base64>"`；ARRAY → JSON 数组；STRUCT/OTHER → string
可读降级。LOB（CLOB/BLOB）单列 **16 MB 上限**，超出报结构化错误（v1 不做流式）。

## 错误归类与排障

JDBC `SQLException` → 宿主结构化信封（`e.code`/`e.retryable`/
`e.sessionDisposition`/`e.data.vendor_code`/`e.data.vendor`），SQLState class 表镜像
oj-db-mysql：

| SQLState class | category | retryable | session |
|---|---|---|---|
| `08xxx` | connection | true | close |
| `23xxx` | constraint | false | keep |
| `40xxx` | timeout | true | keep |
| `42xxx` | syntax | false | keep |
| `53xxx` | capacity | true | keep |
| `57xxx` | cancelled | false | keep |
| `0Axxx` | unsupported | false | keep |
| 其他/厂商码/NULL | internal | false | keep |

达梦/金仓部分错误只给厂商码或 null SQLState——拿不到标准类时**绝不硬套 MySQL
表后静默误判 retryable**，直接 internal；`error_code` 原样留在
`e.data.vendor_code` 供排障。错误消息里的 `user:pass@` 连接串自动脱敏成
`://***@`（host/库名保留——排障要知道连的是哪个库）。

**`e.data.vendor` 方言族诊断名**（v0.1.144）：所有 handle 级结构化错误信封的
`data.vendor` 附 `vendor_of(url)` 识别的方言族（postgresql/mysql/oracle/dameng/
kingbase/mssql + 七方言 snowflake/databricks/clickhouse/trino/kyuubi/heavydb/
duckdb；未知前缀原样透出）——多 vendor 共处部署的排障定位锚点。仅诊断，
不影响 category/retryable 分类。

| 症状 | 解 |
|---|---|
| `java not found (searched in order: plugins.oj-db-jdbc.java_bin → JAVA_HOME/bin/java → PATH)` | 三选一：cfg 指 `java_bin` / 装 JDK 设 `JAVA_HOME` / 加 `PATH`（文案自带三条修复路径） |
| connect 报 `No suitable driver` | `jdbc/drivers/` 目录为空或 jar 放错位置——报错尾部附 stderr 最后 ~20 行与实际扫描路径；`drivers_dir` 可显式指绝对路径。也可能是老旧 jar 缺服务条目 → 信封 `props.driver_class` 逃生舱 |
| `proto mismatch: sidecar proto=… bridge=… — .so 与 jar 必须同版整目录拷贝` | hello 协议严格相等：插件 `.so` 与桥 jar 版本锁死（见「升级须知」），整目录一起换 |
| `sidecar lost: …` / `sidecar shutdown: exited with …` | JVM 死亡（OOME/驱动崩溃/被 kill）：名下全部 handle/stream 立即失效，事务随 JVM 消亡回滚（语义安全）；下一次请求自动重启 sidecar，需 reconnect，**绝不自动重放** |
| insert 多行绑参报「序列号无效」（DM） | values 组数与参数个数没对齐——按组逐一配平（见上表） |
| 查询结果整数列变成 string / DECIMAL 变 number | 正常分流：scale>0 → string（保精度），scale==0 → 整数族；别用 `Number()` 强转大数，用 `toBigInt` |
| sidecar 慢查询钉死 | 非 stream 的 `db.query`/`db.exec` 无语句级取消（看门狗只杀 JS 侧，JVM 里语句继续跑）——强制 `props.socketTimeout`，大结果走 `db.stream`；详见「限制」 |

## 七方言 jar 分发矩阵（v0.1.144，REQ-A4）

> 上游 REQ-A4 七种非原生数据源经本插件接入。代码侧零方言分支（Java 桥通用，
> Rust 侧 `vendor_of()` 诊断表识别方言族）；**工作量与风险全在驱动 jar 的分发**。
> 版本/体积/许可证均为 Maven Central 直接检索实证（2026-11-24），升级任何一行
> 先重跑 `e2e_driver_smoke` 腿。

### 随包三件套（Apache-2.0，允许随二进制再分发；合计 ~45MB）

```bash
cargo xtask jdbc-drivers          # 一键下载 + 归位 <bin>/plugins/<triple>/jdbc/drivers/，幂等（逐 jar 过 Maven .sha1 校验，残缺自动重下）
cargo xtask jdbc-drivers --dir /opt/oj/jdbc-drivers   # 自定义目录
```

> 该缺省目录只是**下载落位**——运行期不会自动发现：sidecar 的 `drivers_dir`
> 须在 config 显式指到同一目录（`--dir` 与 `drivers_dir:` 对齐，或缺省布局下
> 写全绝对路径），见下方自备四步第 2 步。

| 方言 | 坐标 / pinned | jar 体积 | 最低 JDK | 驱动类 | URL 形态 |
|---|---|---|---|---|---|
| Trino | `io.trino:trino-jdbc:484` | 10.6MB | 11+（470 起） | `io.trino.jdbc.TrinoDriver` | `jdbc:trino://host:8080/catalog/schema` |
| ClickHouse | `com.clickhouse:clickhouse-jdbc:0.11.2` **`-all-dependencies`** | 13.8MB | 8+ | `com.clickhouse.jdbc.ClickHouseDriver` | `jdbc:ch:http://host:8123/db`（或 `jdbc:clickhouse:`） |
| Kyuubi | `org.apache.kyuubi:kyuubi-hive-jdbc-shaded:1.12.0` | 21MB | 8+ | `org.apache.kyuubi.jdbc.KyuubiHiveDriver` | `jdbc:kyuubi://host:10009/db` |

三个 jar 均自包含 shaded，**可同目录共处**（e2e_driver_smoke 真 JVM 实证：单
UCL 三驱动同时注册；运行条件 = 桥 jar + 三驱动在场，该腿 env-gated 不入 CI 门禁；
`jdbc:` 诊断方言族 snowflake/databricks/clickhouse/trino/kyuubi/heavydb/duckdb
随错误信封 `data.vendor` 与日志透出）。

**jar 形态陷阱（实测钉死）**：
- ClickHouse **必须 `-all-dependencies`**——`-all` classifier 不内嵌 slf4j，两个
  驱动类实例化即 `NoClassDefFoundError`，DriverManager 报 `No suitable driver`
  （症状与没装 jar 一模一样）；thin 版（0.3MB）缺全部依赖同理。
- Kyuubi **必须 `kyuubi-hive-jdbc-shaded`**——别选 `org.apache.hive:hive-jdbc`
  （thin 171KB，依赖地狱，平面目录不可用）。
- Trino 周级发版 + JDK 基线历史上从 8 提到 11（470 起）——升级 pinned 版本须读
  release notes 并重跑 smoke 腿。

### 自备四件（体积或上游状态所致，oj 不随包）

> 装载证据口径（诚实登记）：HeavyDB/Snowflake/Databricks 的 smoke 臂在
> e2e_driver_smoke 在位（env 注入即实证），本轮无 jar/实例，**未实跑**——
> Snowflake/Databricks 装载证据为 env-gated 待补，HeavyDB 仅承诺 jar 可装载。

| 方言 | 坐标 | jar 体积 | 许可证 | 判定 |
|---|---|---|---|---|
| Snowflake | `net.snowflake:snowflake-jdbc:4.4.1` | **99MB** | Apache-2.0 | 自备/可选包（体积） |
| Databricks | `com.databricks:databricks-jdbc:3.4.5` | 43MB | Apache-2.0 | 自备/可选包（体积；Java 11+） |
| DuckDB | `org.duckdb:duckdb_jdbc:2.0.1.0` | **81MB** | MIT | 自备/可选包（体积 + 内嵌全平台 native）；**离线可用**（内存库零服务端，e2e_duckdb 腿实证全链） |
| HeavyDB | `ai.heavy:heavyai-jdbc:6.1.0` | 4.5MB thin | Apache-2.0 | **上游 2026-05-27 官宣废弃**，Maven 停更，thin jar 缺 Thrift 依赖需 `-Pfatjar` 自构——oj 只承诺通用连接 + 用户自备 jar，e2e 降级为 env-gated 可选腿 |

自备四步（照抄）：

1. 建目录并放 jar（**一目录一版本**——同 vendor 同名不同版先到先得，装载按文件名
   排序）：
   ```bash
   mkdir -p /opt/oj/jdbc-drivers && cp snowflake-jdbc-4.4.1.jar /opt/oj/jdbc-drivers/
   ```
2. config.yaml 指路（缺省相对 CWD 的 `drivers/`，建议显式绝对路径）：
   ```yaml
   plugins:
     oj-db-jdbc:
       drivers_dir: /opt/oj/jdbc-drivers
   ```
3. DSN 用 JSON 信封形态（凭据不进 URL、可整串 `ENC[...]` 密封）：
   ```yaml
   db:
     sf: 'jdbc:{"url":"jdbc:snowflake://acct.snowflakecomputing.com/?warehouse=W","user":"oj","password":{"$env":"SF_PW"}}'
   ```
4. 老驱动缺 ServiceLoader 条目时加 `"props":{"driver_class":"<驱动类名>"}` 逃生舱；
   改 jar 后**必须重启 oj 进程**（JVM 一次性装载）。

多实例一致性（REQ-A4 验收第 5 条口径）：config 声明式 DSN 各实例同 config 天然
一致；运行期 `db.client.register` 登记进进程内 StableState，池内各 runtime 同见
（v0.1.92 既有行为）。连通性测试（验收第 3 条）：`DB("name").query("select 1")`
即探针，无专用端点（能力已有，不造重复面）。



- **SQL 业务全责**：裸 SQL + `?` 绑参是主路径——注入防线照旧（值只走绑参；
  动态标识符在 jdbc 后端没有构造器白名单可用，只能走 schema.yaml 自建白名单 +
  服务端校验，别拼用户输入）。
- **多厂商共处口径**：单 JVM 单 URLClassLoader 装载 drivers 目录全部 jar。
  随包三件套（Trino/ClickHouse/Kyuubi，自包含 shaded）已实证可同目录共处
  （e2e_driver_smoke 真 JVM 三驱动同时注册）；**非自包含驱动**（DM/金仓/Oracle
  等易冲突厂商）仍建议「每个部署只放一家驱动」——类冲突/静态状态串扰是 JDBC
  生态老问题。混放多厂商时遵守「一目录一版本」（同 vendor 同名不同版先到先得，
  装载按文件名排序）。
- **drop-in 新 jar 需重启 oj 进程**：驱动类静态状态 + URLClassLoader 不可卸载
  （JVM 由 sidecar 持有，宿主无法热替换）。
- **首 connect 承担 JVM 冷启动**：sidecar 惰性 spawn，第一个 JDBC 请求付 JVM
  启动账（实测：达梦 e2e 全链含 JVM 冷 spawn 1.8s；H2 冒烟腿全链亚秒级）。部署
  手册建议**预热**：启动后跑一次健康检查查询再挂流量。
- **慢查询钉死**：非 stream 查询无取消；sidecar cached 池 32 是长查询并发硬顶。
  连接属性显式给 `socketTimeout`（毫秒），后果是超时语句按 `40xxx`→timeout
  分类、retryable。
- **流式 cancel 的诚实口径**：`stream`/`cancel` 能力位真实有效（H2/金仓可达），
  但**达梦 8.1.1.49 实测 mid-fetch cancel 结构性不可达**（服务端计算全在
  open 期、传输期 cancel 被驱动串行化成 no-op）——表现为「cancel 被接受、
  无效果」。宿主协作式取消契约不受影响（下一批停拉）。
- **UTC 时区口径**：sidecar 恒带 `-Duser.timezone=UTC`；DATE/TIMESTAMP 读回
  是 JVM-UTC 的 ISO-8601 串（`Z` 尾），标准 DATE 列带 `00:00:00` 是表示层扩展。
- **`''` 读回是驱动特定行为**：Oracle 系吞成 NULL，达梦 8 实测原样保留空串——
  跨厂商代码对空串列做显式归一（`v === null ? "" : v` 之类的口径由业务定）。
- **DDL 隐式提交**（Oracle/达梦）：事务内 DDL 会断事务，rollback 不回滚 DDL——
  DDL 一律 `db.exec` 放事务外跑。
- **LOB 单列 16 MB 上限**；大结果全量缓冲在 `-Xmx512m`（缺省）内——大结果集
  必须走 `db.stream`/`db.export`。
- **多租户共享 JVM**：连接级无跨租户泄漏（错误消息随请求走、凭据已脱敏），
  但驱动写 stderr 的日志可能含他租户 SQL/连接串，落宿主共享日志槽位——按共享
  日志的既有管控处理。
- **宿主 SIGKILL/crash → 孤儿 JVM**：sidecar 成孤儿、持有 DB 连接至
  `socketTimeout`——桥的 EOF 有序退出契约覆盖正常停机，异常死亡靠
  `socketTimeout` 收窄。
- **目标机必须有 Java 11+**：oj 的「拷贝即跑」承诺对本插件附加此前提
  （JRE + 厂商 jar）。
- **DuckDB 全量内存无流式**（v0.1.144 实测）：结果集全量物化，桥的
  `setFetchSize(500)` 无流式效果——大查询顶 JVM 堆，`db.stream` 对它只是
  分批搬运不是省内存；cancel 仅批间生效。内存库 `jdbc:duckdb:` 离线可用。
- **DuckDB 约束冲突不回 SQLState**（v0.1.144 实测）：唯一键冲突 vendor_code 0、
  无 23 类 SQLState → errmap 落 `internal` 兜底（`retryable:false` + 结构化
  信封契约仍成立）；依赖 SQLState 分流的调用方对 DuckDB 需按 message 判别。
- **DDL 受影响行数跨厂不一**：DM/H2 回 0，DuckDB 回 -1（SUCCESS_NO_INFO）——
  DDL 成败以不抛错为准，勿按计数分支。
- **Trino connect 是懒连接**（v0.1.144 实测）：connect 只建对象不开 TCP——
  对不可达端点 connect 也会成功，网络错误落在首个查询（如 `select 1`）。
  健康探针勿以 connect 成功为连通判据，至少补一刀查询。

## 升级须知：`.so` 与 jar 版本锁死

插件 `.so` 与 `oj-jdbc-bridge.jar` **必须同版、整目录一起拷贝**——hello 帧协议
严格相等（`PROTO=1`），不匹配报两侧版本号并拒绝运行；升级时 `bin/plugins/
<triple>/oj-db-jdbc` 与 `bin/plugins/<triple>/jdbc/` 一起换，不要只换其一。
`cargo xtask plugin db-jdbc --check` 做 ABI / 身份 / semver / 符号 / jar 预检
（缺 jar 直接报错点名修法）。厂商驱动 jar 由部署方自控版本，与桥 jar 无锁。

## 状态

v0.1.141 首次发布。验证面：Rust 单测 35（含跨平台假 java 夹具）+ Java 桥 JUnit
（H2 真绑 golden fixture）+ H2 真 JVM 冒烟腿 + 达梦远程 e2e 主腿（1.8s 全绿）+
金仓可选腿（env-gated，诚实 skip）；Oracle 不在 v1 验证范围（协议通用，接入路径
与达梦同形）。可运行样例：`sample/src/dm/`（达梦 env-gated 查询模块）。
