# 02 · 配置模型（`src/config.rs`）

## 1. 治理原则

**块存在即启用，块缺失即禁用。** 全文件单源（`config.yaml`），无 env 叠加。
`Option<T>` 的 `None` = 能力不装配（对应全局/路由/守卫都不挂）。

加载：`config::load_from(dir, explicit)`（`config.rs:322`）
- `explicit = Some(p)` → 文件不存在即 **Err**；
- `explicit = None` → 找 `config.yaml`，缺失**静默用默认值**。

## 2. 字段树

### `server`（`ServerCfg`，全部有默认）

| 字段 | 默认 | 说明 |
|---|---|---|
| `host` | `"localhost"` | |
| `port` | `9778` | 与 README / `sample/config.yaml` 一致；由 `config.rs` 的默认值单测钉死 |
| `base` | `"/v1/api"` | API 前缀；CLI `-b` 覆盖；空前缀拒绝（避免全 404 静默坑） |
| `app_path` | `None` | 静态站点根（相对 config 目录）；`None` = 不开静态服务 |
| `timeout` | `"30s"` | 单请求执行超时，超时 → 408 |
| `pool_size` | `4` | JS 执行并发度（= actor 数） |
| `max_upload_bytes` | `10 MiB` | 超出 → 信封 413；axum 层 2x 硬顶（裸 413） |
| `logs_dir` | `None` → `<config>/logs` | 每次启动一个 `server-<秒>_<pid>.log` |
| `logs_max_m` / `logs_keep_files` | `100` / `10` | 单文件上限（<100 按 100）/ 保留个数（最小 2） |
| `console_log` | `false` | 默认只落盘，终端保持干净；`--console-log` 打开 |
| `public_key_path` / `certificate_path` | `""` | **证书必配**，两路径缺任一 → 装配拒绝启动 |
| `grace_days` | `30` | 过期后宽限天数（期间服务起得来，但 GET 被限） |
| `migrate_on_start` | `None` | `auto` / `verify` / `off`；缺省按模式取：dev=auto、release=verify |
| `ownership_guard` | `None` | `warn`（默认，违规仅告警） / `deny`（违规拒绝）；非法值 fail-fast |

### 能力段（均为 `Option`，`None` = 不启用）

| 段 | 类型 | 语义 |
|---|---|---|
| `db` | `HashMap<name, DSN>` | 多库混用（`sqlite://` / `mysql://` / `postgres://`），经 `DbBackendRegistry` 按 scheme 认领 |
| `redis` | `HashMap<name, URL>` | 仅 `redis.default` 参与装配（其余 warn 忽略）；有声明但无 kv 插件 → fail fast；未声明 → 内置 `InMemoryKV` |
| `tenant` | `TenantCfg` | `enable` + `header_key`（默认 `X-TENANT-ID`）+ `anonymous_paths`（尾 `/*` 一层通配） |
| `auth` | `AuthCfg` | `jwt_secret`（空 → fail fast）、`signing_method`(HS256/384/512)、access/refresh 时长、`anonymous_paths` |
| `oidc` | `OidcSection` | `issuer` / `private_key_path`（相对 config 目录）/ `rp: {tenant → {issuer, client_id, client_secret, scope}}` / `clients: {id → {secret, redirect_uris, tenant}}` |
| `blob` | `BlobSection` | 平铺字段 = 旧单后端（等价 `backends.default`）；`backends.<name>` = 命名多后端；**两者并存且平铺非默认 → 歧义 Err** |
| `es` | `EsCfg` | `endpoint` |
| `broker` | `BrokerCfg` | `kind`: local/kafka/rabbitmq；`brokers` / `url` / `group` / `topic_prefix` |
| `plugins` | `HashMap<name, cfg>` | **一段三用**：键 = 严格清单（非空 map 只装配列出的）/ 值 = 透传 cfg（非空对象原样透传，空对象回落轴适配器）/ 缺省或空 map = 扫描模式。旧 list 写法解析报错 |
| `plugins_dir` | `Option<PathBuf>` | 相对 config_dir；`None` 走四级后备（见 [05](05-ffi-and-plugins.md)） |

## 3. 时长解析

`parse_duration`（`config.rs:345`）支持 `s|sec|secs`、`ms`、`m|min`、`h`、`d`（`f64` 数值）。
非法单位/数值 → Err。

## 4. 校验与 fail-fast 落点

| 校验 | 位置 |
|---|---|
| 证书两路径齐备 | `ServerCfg::cert_paths_configured()` + `oj/src/app.rs:113` |
| `blob` 平铺/命名歧义 | `BlobSection::entries()` |
| `auth.jwt_secret` 非空 | `oj/src/app.rs:241` |
| `migrate_on_start` / `ownership_guard` 非法值 | `oj/src/app.rs:173-192` |
| `server.base` 为空 | `oj/src/server_cmd.rs:95` |
| 模块名/版本白名单 | `oj/src/manifest.rs:25,37` |
| schema.yaml 标识符白名单 `[A-Za-z_][A-Za-z0-9_]*` | `oj/src/schema.rs:103` |

## 5. 已知问题

- ~~默认端口 778 与文档不一致~~ —— 已于 2026-09-06 整改：默认改为 `9778`
  （`config.rs` 默认值单测 `defaults_when_no_file` 钉死）。
- `Config` 无 `boot` 字段：`StableState.boot` 来自 `oj/src/app.rs:67` 的
  `ext_boot_spec()`（探测 `<config_dir>/ext_boot.js`），与 config 解耦。
- `BlobCfg` 与 `BlobSection` 字段重复（`BlobSection` 内嵌一份平铺 + `backends`），
  是为兼容旧格式；新增字段需两处同步。
- 本文件近半是单元测试（`config.rs:360-661`），覆盖了默认值、证书门禁、blob 歧义等，质量好。
