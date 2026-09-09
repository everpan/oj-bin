---
title: 01 · 这是什么
updated: 2026-09-08
---

# 01 · oj 是什么

## 一句话

oj 是一个把 **V8（通过 deno_core）嵌在 Rust 里**的低代码后端框架：你用 JS/TS 写业务
handler，Rust 负责兜底能力（数据库、缓存、对象存储、消息队列、鉴权、证书），两边通过
一批注入的全局对象（`db`、`kv`、`http`、`json`…）打交道。

## 它不是什么

先把边界说清，能省掉很多错误期待：

- **不是 Node.js 运行时**。`require('fs')`、npm 生态里依赖 Node 内置模块的包跑不起来；
  纯 ESM 包可以直接 vendor 进项目用（sample 的 `escape-goat` 就是这么用的）。
- **不是 ORM 框架**。没有模型/实体/关联这一层，你写的是 SQL + 绑定参数，
  框架只保证「表名列名来自白名单、值走绑定参数」。
- **不是微服务框架**。单进程 + 插件 cdylib，水平扩展靠多实例 + Redis（kv/会话）
  与消息队列（bus/mq），框架不替你做服务发现。
- **不是「配置即后端」的零代码平台**。低代码指的是不用写路由表、不用搭脚手架，
  业务逻辑仍然是正经 JS/TS 代码。

## 一次请求穿过哪七段

新人脑子里有这条链路，后面看任何文档都不会迷路（完整版见[模块地图的请求链路](../modules/index.md)）：

```
HTTP 请求
 └─ ① 证书门禁        证书过期且过了宽限期 → 403，业务代码根本不会执行
 └─ ② 静态/下载       命中 {base}/blob/{key} 或 app_path 静态文件就在这层返回
 └─ ③ 路由查表        RouteTable 按「目录镜像」规则匹配（matchit）
                      四态：命中 / 冲突(500) / 方法不允许(405) / 未找到
 └─ ④ 前置管线        鉴权守卫验签 → 401 或注入 http.user
                      租户头提取 → 400 或注入 http.tenantId
                      体积上限 413 + multipart 解析
 └─ ⑤ 派发到 JS 线程  JsActor：每个 runtime 跑在专用线程上（JsRuntime 是 !Send）
 └─ ⑥ 执行 handler    RuntimePool 取出 V8 isolate → 重置 ReqState → 看门狗上弦（超时 408）
                      → import api.ts → 调 default[method]() → 你的代码
 └─ ⑦ 写回信封        {code,msg,data}；协议类端点可用 json.raw 出裸 JSON
```

## 三根支柱

| 支柱 | 谁写 | 说明 |
|---|---|---|
| 业务逻辑 | 你（JS/TS） | `api.ts` 里导出 `get`/`post`/… 就是接口，目录即路由 |
| 运行时与能力 | 框架（Rust） | 全局对象、runtime 池、迁移、路由、证书门禁 |
| 后端实现 | 插件（cdylib） | db / kv / blob / bus / es / auth / mq 七个轴，按需加载 |

第三根是最容易被忽略、也最能体现设计取向的一点：**核心库不依赖任何具体后端**。
`src/` 只认 trait，mysql、postgres、redis、s3、kafka、rabbitmq 全在 `plugins/*` 里，
不装就不进二进制、不进依赖树。这也意味着「加一个新后端」不需要动核心。

## 什么时候该用 oj

合适：内部系统 / 中后台 API、需要多租户与统一鉴权的业务、想让前端同学也能写后端接口、
需要一个带迁移与证书门禁的、可交付给客户自部署的小后端。

不合适：重 CPU 的计算服务（V8 不是主场）、需要庞大 Node 生态的项目、超低延迟网关。

## 下一步

- [02 · 构建与运行](./02-install-build.md) —— 先把 sample 跑起来
- 想先看看成品长什么样：[sample 项目](../sample/index.md) 与[模块导览](../sample/modules-tour.md)
