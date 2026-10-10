# plugins —— 插件自省

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`plugins()` 返回**已加载 cdylib 插件**的清单（`src/bridge/plugins_op.rs`），供升级核对
与运维自省：插件升级时对比 `abi_version` 与宿主 `host_abi_version`（严格相等门禁），
`fingerprint` 用于确认「线上跑的就是我构建的那份」。

同一清单还经内置公共端点 **`GET {base}/plugins`** 公开（保留路径，会遮蔽同名业务路由；
不走 Bearer，运维/监控直接 curl 即可）——JS 的 `plugins()` 与该端点同源。

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `plugins` | `plugins(): PluginInfo[]` | **同步**返回已加载插件数组（无 IO，装配期冻结）；零插件 → `[]` |

`PluginInfo` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | `string` | 插件名（如 `db-mysql` / `kv-redis` / `auth`） |
| `semver` | `string` | 插件版本号 |
| `abi_version` | `number` | 插件构建时的 ABI 版本 |
| `fingerprint` | `string` | 构建指纹（核对外发产物一致性） |
| `description` | `string` | 插件作者自述（descriptor 必填项） |
| `host_abi_version` | `number` | **宿主**当前 ABI 版本（每条记录重复携行，方便逐条比对） |

## 错误

`plugins()` 不抛错：

| 场景 | 行为 |
|---|---|
| 零插件加载 | 返回 `[]`（不是 `null`、不抛错） |
| ABI 不匹配的插件 | **根本进不了清单**——装配期严格相等门禁 fail-fast，启动即拒绝 |

## 限制

- 清单是**装配期快照**：运行期 `dlopen`/卸载不存在，返回值在进程生命周期内不变。
- 只报告**插件轴**（es/db/blob/bus/kv/auth/mq/mail/ldap 经 cdylib 加载的后端）；核心
  内建轴（`json`/`http`/`fs`/`log`/`vars` 等）不在清单内。
- **轴清单自报（v0.1.54）**：插件经 `oj_plugin_axes()` 宏双发声明自己的轴（类型化 +
  泛型）；宿主探测自报清单优先，旧插件（无该符号）回落逐轴 dlsym（deprecated 告警，
  免重编兼容）。泛型轴名清单见 `ojInfo().generic_axes` / `oj info` 的 `generic_axes` 行。
- `GET {base}/plugins` 是保留路径：业务模块不要建同名路由（会被遮蔽）。
- 字段语义用于**核对**，不要拿 `semver` 做功能开关（功能探测用「调用看报错」，
  如 `es not configured`）。

## 案例

### 运维自省端点（业务包装）

```ts
// src/ops/plugins/api.ts —— 包一层业务鉴权后透出（公共端点 {base}/plugins 不走 Bearer）
function get() {
  if (http.user?.roles?.includes("admin") !== true) {
    json.fail(403, "admin only"); return;
  }
  json.ok(plugins());
}
export default { get };
```

```bash
curl http://localhost:9778/v1/api/plugins        # 内置公共端点（无需 token）
# → {"code":0,"msg":"ok","data":[
#     {"name":"db-mysql","semver":"0.1.53","abi_version":11,
#      "fingerprint":"…","description":"MySQL db backend","host_abi_version":11}, …]}
```

### 升级核对：ABI 一致性自检

```ts
// src/ops/abicheck/api.ts —— 发布巡检：任何插件 ABI 落后宿主即报警
function get() {
  const bad = plugins().filter((p) => p.abi_version !== p.host_abi_version);
  if (bad.length) {
    log.error("plugin ABI drift", "plugins", bad.map((p) => p.name).join(","));
    json.fail(500, "plugin ABI drift", bad.map((p) => p.name));
    return;
  }
  json.ok({ count: plugins().length });
}
export default { get };
```

### 能力探测 + 指纹留痕

```ts
// src/ops/env/api.ts —— 部署核验：记录线上实际插件指纹，确认发的是对的构建
function get() {
  const ps = plugins();
  log.info("env check", "plugins",
    ps.map((p) => `${p.name}@${p.semver}#${p.fingerprint.slice(0, 8)}`).join(" "));
  json.ok(ps.map((p) => ({ name: p.name, semver: p.semver })));
}
export default { get };
```
