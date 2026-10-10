# ojinfo —— 装配期固化诊断（`oj info` CLI 与 JS `ojInfo()`，v0.1.54）

> 本文档由 `oj/src/serve_cmd.rs`（`assemble_ojinfo`）梳理生成；权威 API 参考见
> ../devkit/api-manual.md §6「ojInfo()」。

## 是什么

phpinfo 风格的**装配期声明面诊断**。两个出口共用同一装配体
（`serve_cmd::assemble_ojinfo` 单一事实源）：

- **CLI**：`oj info [-c config.yaml]` —— 五段 php -i 风格纯文本，进程内打印后退出；
- **JS**：handler 里 `ojInfo()` —— **同步**返回同源的 JSON 对象（装配期冻结进
  `StableState`，无 IO）。

典型用途：升级核对（插件 abi/semver vs 宿主）、部署核验（build 段固化「线上跑的是
哪份二进制」）、配置巡检（段名清单 + unconsumed 段诊断）。

## 五段结构

| 段 | 内容 | 零值泄漏语义 |
|---|---|---|
| `build` | `oj` 版本、`profile`（release/debug）、`host_triple`、`v8` 版本、`exe` 绝对路径、（CLI 时）`config_path` | 无敏感面 |
| `abi` | `abi_version`（当前 11）、`host_fingerprint`（rustc/契约 crate/triple 指纹） | 无敏感面 |
| `plugins` | 每个已加载插件 `{name, semver, abi_version, fingerprint, description, host_abi_version}` | desc 为插件作者自述，勿写机密 |
| `backends` | 声明面：`db_schemes.declared`（config `db:` 键）、`blob_configured`、`kv_plugin`/`auth_plugin`/`mail_plugin`/`ldap_plugin`/`es_plugin`（槽位有无）、`mq_plugins`、`bus_kinds`、`dbs_registered` | **不 connect** 任何库/broker——纯注册表快照 |
| `config` | `sections`（顶层 config 段名排序清单）+ `unconsumed`（未被消费的段名） | **只出键名不出值**——DSN、`bind_pw`、`ENC[...]` 明文不可能经此泄漏 |

顶层另有 `generic_axes`（已注册泛型轴名清单）与 `unconsumed_sections`（与
`config.unconsumed` 同源）。

- **unconsumed 段诊断**：顶层 config 段既不在宿主已知段表、也没被任何已加载插件消费
  （消费 = 插件自报 `config: "key"` 命中该段，或 `plugins:<name>` 非空透传）→ 启动时
  stderr 打 `[oj-serve] unconsumed config sections: […] (typo? or plugin not loaded)`。
  典型成因：段名拼错（`cahce`），或对应插件没装/没进清单。
- CLI 首行即声明红线：`oj info — declaration surface only (no connections, no config values)`。

## 错误

| 场景 | 行为 |
|---|---|
| `-c` 指向坏 config / 插件目录加载失败 | 非零退出，错误前缀 `config:` / `plugins manifest:` / `plugins scan:` 指明阶段 |
| CLI 副作用 | 会执行插件 `init`（与 serve 同一信任边界），但不 connect、不监听端口 |
| JS `ojInfo()` 未注入（理论缺省） | 返回 `{}`（空表而非 None，与 `vars` 同哲学）——正常装配路径恒有值 |

## 限制

- **没有公共 HTTP 端点**（与 `plugins()` 的 `GET {base}/plugins` 不同）：要对外透出就
  自己包一个业务路由（加业务鉴权），handler 内 `json.ok(ojInfo())` 即可。
- 快照随进程生命周期不变：运行期 `dlopen`/卸载不存在，重查须重启。
- CLI 不替代 `plugins()` 做运行期升级核对——`plugins()` 可在 handler 里逐请求调，
  `oj info` 是运维侧的启动前/线下诊断。
- `backends` 只报**声明面**：「配了 redis 段」≠「连得上」——连通性 probe 不在本工具范围。

## 案例

### 运维：升级前核对插件 ABI 与构建

```bash
./bin/oj info -c config.yaml
# oj info — declaration surface only (no connections, no config values)
#
# ## build
# oj: 0.1.54
# profile: release
# host_triple: aarch64-apple-darwin
# v8: 15.0.245.2-rusty
# exe: /…/bin/oj
# config_path: config.yaml
#
# ## abi
# abi_version: 11
# host_fingerprint: rustc-…
#
# ## plugins
# - kv-redis 0.1.0 (abi 11)
#   desc: kv 轴 Redis 插件：redis crate 异步客户端
# - ldap 0.1.0 (abi 11)
#   desc: LDAP directory search + bind-as-auth (ldap3); generic axis, requires host >= v0.1.54
# generic_axes: ["ldap"]
```

### handler 内自省（进程内，无公共端点）

```ts
// src/ops/env/api.ts —— 部署核验：线上实际装的是什么（config 只出键名，可安全外发）
function get() {
  if (http.user?.roles?.includes("admin") !== true) {
    json.fail(403, "admin only"); return;
  }
  const info = ojInfo();
  json.ok({
    oj: info.build.oj,
    plugins: info.plugins.map((p: any) => `${p.name}@${p.semver}#${p.fingerprint.slice(0, 8)}`),
    genericAxes: info.generic_axes,
    unconsumed: info.unconsumed_sections,   // 配置拼写巡检
  });
}
export default { get };
```

```bash
curl -s http://localhost:9778/v1/api/ops/env/ -H "Authorization: Bearer $TOKEN"
# → {"code":0,"data":{"oj":"0.1.54","plugins":["kv-redis@0.1.0#abcd1234","ldap@0.1.0#…"],
#     "genericAxes":["ldap"],"unconsumed":["cahce"]}}
# unconsumed 非空 → config 里有没被消费的段（拼错或插件没装）
```

### 配置巡检：unconsumed 段的两种复查路径

```bash
# 启动日志（stderr）
[oj-serve] unconsumed config sections: ["cahce"] (typo? or plugin not loaded)

# 线下复查（不启服务）
./bin/oj info -c config.yaml | grep -A2 '^## config'
# sections: […, "cahce", …]
# unconsumed: ["cahce"]
```
