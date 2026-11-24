# oj-predict —— 预测执行与合规网关（REQ-PREDICT，v0.1.145）

上游需求 `docs/UPSTREAM-REQS.md` §3.2：平台接受一段 PQL（Predictive Query Language）
加明确的数据范围与授权，在**受控数据面**执行并回给模块，全程审计留痕。对应验收五条
（受控执行 / dataScope 越界拒 / consent 未过不执行且留审计 / 结果可回传 ReAct /
失败超时可观测不污染租户数据）。

## 30 秒上手

```yaml
# config.yaml
plugins_dir: bin/plugins        # oj-predict 已在 bin/plugins/<host-triple>/
predict:
  timeout_ms: 15000             # 引擎调用上界（毫秒）
  policy:
    allowed_sources:            # dataScope 白名单（精确或 db:<库名>.* 尾通配）
      - "db:crm.*"
    grants:                     # 部署期授权表（静态治理，见「合规边界」）
      - id: "g-forecast-2026"
        scopes: ["forecast.monthly"]
        not_after_ms: 1798761600000   # unix 毫秒过期线；缺省 = 永不过期
```

```ts
// handler（同步 Promise，纯 JSON 进出——可直接当 ReAct 工具输出）
const out = await predict.run({
  pql: "FORECAST revenue NEXT 3",
  dataScope: { sources: ["db:crm.orders"], maxRows: 10 },
  consent: { grantId: "g-forecast-2026", scopes: ["forecast.monthly"] },
});
// out.result  → 引擎返回（mock 引擎：{engine:"mock", rows:[{period,value}…]}）
// out.trace   → { id, status:"ok", engineMs, grantId, sources, ticket, violations:[] }
```

不装 oj-predict（或任何契约兼容引擎）时 `predict.run` 报
`OJ_PREDICT_ENGINE`「predict engine plugin not assembled」——**能力缺席，绝不静默降级**。

## 网关三闸（全部宿主侧，插件不可绕过）

| 闸 | 校验 | 失败形态 |
|---|---|---|
| dataScope | 声明源逐条对 `allowed_sources`（精确串，**区分大小写** / `db:<库名>.*` 尾通配）；**任一条越界 → 整体拒绝**（绝不裁剪放行），violations 点名全部越界项。`dataScope` 缺省或 `sources` 空数组 = 声明零触达、恒过（闸 2 照常） | `OJ_PREDICT_SCOPE`（`e.data.violations`）+ 审计 `PREDICT_DENY` reason=scope |
| consent | grant 存在 → 未过期（`now >= not_after_ms` 即过期，fail-closed）→ 声明 scopes ⊇ 请求 scopes | `OJ_PREDICT_CONSENT`（`e.data.reason`: no_such_grant / expired / scopes_insufficient）+ 审计 `PREDICT_DENY` reason=consent.* |
| 引擎 | 收编轴在位；`timeout_ms` 有界（装配期 `0` 拒启）；引擎须回显网关 ticket（契约校验） | `OJ_PREDICT_ENGINE` + 审计 `PREDICT_AUDIT` status=timeout / error |

**审计**（U-9 同一出口，`audit.query({kind:"PREDICT_AUDIT"|"PREDICT_DENY"})` 可查）：
成功、失败、超时、**拒绝**、连 payload 不可解析（AUDIT denied reason=bad_payload）都
留痕——detail 按 status 分形（denied 另带 reason/violations；timeout 另带 timeoutMs；
error 中契约违例另带 reason），零凭据，PQL 原文不出审计（只出 `pqlSha256` 前 12 hex），
**也不落引擎消息文本**（msg 只回 JS）。

**脱敏**：引擎错误消息经网关剥 PQL 原文精确子串 + 300 字符截断后才回 JS——只剥
**精确整条**回显，引擎把 PQL 拆散/变形拼进错误文案时仍可能残留 ≤300 字符，故引擎
契约同时规定**错误消息不得嵌入 PQL/请求数据**（部署侧适配器必须遵守）。

## 引擎轴收编（特权旁路封堵）

轴名 `predict`（config `predict.axis` 可改）在装配期**收编出 `axis()` 公开面**
（`mgmt.axes` 同款机制）：handler 里 `axis("predict")` 直接报
`unknown generic axis 'predict'`，`ojInfo().generic_axes` 也不含它。引擎只能经
`predict.run` 网关触达——三闸不可绕过。

装配期两道 fail-fast（G2 评审收口）：
- `predict.axis` 与 `mgmt.axes` **重叠** → 拒启——重叠会让引擎静默归 mgmt，
  `mgmt.call` 可零三闸触达引擎（合规旁路）；
- 跨插件同名轴（第二家 predict 引擎同名注册）→ 拒启（单轴单提供者）。

## 引擎契约（部署侧自备引擎照此实现）

任何实现以下契约的泛型轴插件即可替换内置 mock（轴名 = `predict.axis`，缺省
`predict`；跨插件同名冲突装配期 fail-fast）：

- 入口：`generic(predict) => &VT`，`config: "predict"`；descriptor name = 插件名。
- op `run`：收**裸对象** `{"pql":…, "dataScope":{"sources":[…],"maxRows":n}, "ticket":"gt-…"}`，
  回 `{"result": <任意 JSON>, "ticket": <原样回显>}`——ticket 不回显 = 契约违例
  （网关拒收）。耗时工作在自己的 runtime 异步做（`catch_future` + `spawn_ffi_future`，
  `call` 快速返回——同步阻塞会拖死网关计时器）。
- 语义要求：**只读预测**（无租户写路径）；遵守 `dataScope.maxRows`；**pql 须非空
  字符串**（内置 mock 对空 pql 报错）；**错误消息不得嵌入 PQL/请求数据**（网关脱敏
  只剥精确整条回显，契约约束是第二道防线）；PQL 语义归引擎，网关不解析 PQL
  （dataScope 是声明式范围，引擎须自律在声明范围内取数）。

内置 mock 引擎（`plugins/oj-predict`）：确定性（sha256(pql) 播种线性趋势，同 pql
恒同值）、无外呼、不触租户数据——离线联调与部署冒烟用。

## 合规边界（红线）

- **handler 自建 `fetch` 外呼预测服务 = 绕过合规治理**（REQ-PREDICT 背景原文约束）：
  租户数据出境必须走 `predict.run` 三闸，平台审计才有痕迹。
- v1 授权表是**部署期静态治理**（config `grants`），不是交互式 consent 流——用户
  在线授权页属上游 G2 合规面裁定，落地后另立需求。
- `dataScope` 是**声明式**范围：网关校验声明 ⊆ 白名单，不解析 PQL 内容验证取数
  与声明一致——引擎契约要求引擎自律；严格一致校验（解析 PQL 对账 scope）留待
  上游裁定后跟进。
- 平台 `fetch` 全局目前**无出站白名单**（既有事实）——出境管控的完整闭环需独立的
  出站白名单需求，不在本阶段范围。

## 限制

- 审计走进程内有界环形（`audit:` 段 `max_entries`/`retention_secs`），不落库——
  长期留存由部署侧按既有日志管道处理。
- 引擎超时后底层引擎侧计算可能仍在跑（网关放弃等待，非取消语义）——外部引擎适配器
  应自带服务端超时。
- 同一 oj 进程一个预测引擎（单轴单提供者）；多引擎按 `predict.axis` 区分需多实例部署。
