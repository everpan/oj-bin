# oj sample — user/order/file

  cargo run -p oj -- server -c sample/config.yaml --api-path sample/src         # dev（TS，热重载；启动自动迁移）
  curl http://localhost:9778/v1/api/user/account/?id=1

  cargo run -p oj -- build -d sample/src -o sample/dist            # 构建（版本目录+migrations+锁+tgz）
  cargo run -p oj -- build --check -d sample/src                   # 结构检查（S002–S006，CI 门禁，不落盘）
  cargo run -p oj -- migrate -c sample/config.yaml -d sample/dist  # release 部署先迁移（verify 门禁要求）
  cargo run -p oj -- schema diff -c sample/config.yaml             # 声明 vs 实库对账（漂移 exit 1）
  cargo run -p oj -- server -c sample/config.yaml --api-path sample/dist   # release（按锁聚合；账本落后拒启）

- 路由 = 目录镜像：src/user/profile/detail/api.ts → /v1/api/user/profile/detail/
- 声明式表结构：每模块 `schema.yaml`（§4.2）声明表/列/索引 → 归属图（表→模块单射，
  同表双声明拒启）+ SchemaRegistry 列白名单（`db.table()` 构造器可用）。
  安全前向（缺表 CREATE / 缺可空列 ALTER / 缺索引）在 dev 启动与 `oj migrate` 自动收敛；
  NOT NULL 新增、疑似改名 fail-fast 并打印迁移模板。sample 四模块（_platform/user/
  order/cert）均已声明
- 表归属守卫：SQL 里出现的他模块表须在 manifest `deps:` 声明（order → user 即演示）；
  `server.ownership_guard: warn`（默认，仅告警）| `deny`（违规拒绝执行）
- 迁移：每模块 `migrations/{seq:04}__{desc}[.方言].sql`（DDL 演进，账本
  `_oj_migrations_<module>`）；`seed.sql` 为幂等参考数据随启动重放；
  `fixtures/` 仅 `oj test` / `oj fixture` 灌入。`server.migrate_on_start`：
  auto（dev 默认）| verify（release 默认，账本落后拒启）| off
- WS 订阅发布示例：连 /v1/api/news/ws 发任意一帧（src/news/WS.ts 订阅 news），
  再 POST /v1/api/news → 连接收到 {"topic":"news",…} 广播帧
- config.yaml `server.app_path: dist`（CLI `--app-path` 可覆盖）：API 未命中的 GET/HEAD 落静态（/manifests.yaml、
  /user-0.1.0.tgz 可直接访问；dist 无 index.html 故 / 为 404）
- 证书必配（不可绕过）：sample 自带自签示例证书 `config/{public.pem,cert.jws}`
  （私钥 `config/private.pem` 仅示例用，**严禁用于生产**；过期后用
  `cargo run -p oj-cert -- renew -k sample/config/private.pem` 重签）
- dist/ 为 oj build 产物（保留原名原结构，默认 minify），可再生，勿手改
- node_modules/escape-goat 为直接 vendor 的纯 ESM 包（可 npm install 替换）
- db.sqlite 由迁移 + seed.sql 初始化（均幂等），已 gitignore
