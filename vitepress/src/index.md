---
layout: home
updated: 2026-09-08

hero:
  name: only-js
  text: 开发者手册
  tagline: 用 JS/TS 写业务，Rust 兜底能力，后端能力按插件插拔
  actions:
    - theme: brand
      text: 从这里开始
      link: /guide/01-what-is-oj
    - theme: alt
      text: 跑通第一个接口
      link: /guide/03-first-api
    - theme: alt
      text: JS API 手册
      link: /reference/api-manual/

features:
  - icon: 🧭
    title: 路由即目录
    details: src/user/profile/api.ts 自动变成 /v1/api/user/profile/，不用写路由表。
  - icon: 🧩
    title: 能力可插拔
    details: 数据库、缓存、对象存储、消息队列、ES 都是 cdylib 插件，不装就不进二进制。
  - icon: 🔒
    title: 安全默认值
    details: SQL 标识只走 SchemaRegistry 白名单、值只走绑定参数，证书门禁无逃生口。
  - icon: ⚡
    title: V8 池化
    details: deno_core 运行时复用 + 看门狗超时（408），热重载只重载改动的模块。
---

## 三条阅读路线

| 你是谁 | 读这些 |
|---|---|
| 第一次听说 oj | [这是什么](./guide/01-what-is-oj.md) → [构建与运行](./guide/02-install-build.md) → [第一个接口](./guide/03-first-api.md) |
| 要写业务接口 | [模块解剖](./guide/04-module-anatomy.md) → [全局对象速查](./guide/05-globals-tour.md) → [JS API 手册](./reference/api-manual/index.md) |
| 要改 oj 本身 | [内部实现走读](./guide/11-internals.md) → [模块地图](./modules/index.md) → [插件开发](./reference/plugin-development.md) |

## 最常被问的三件事

1. **一定要配证书吗？** 是的，`server.public_key_path` 与 `certificate_path` 缺一即拒绝启动，
   没有 CLI 逃生口。sample 自带示例证书，过期用 `oj-cert renew` 重签。见[构建与运行](./guide/02-install-build.md)。
2. **dev 和 release 怎么切？** 不看参数，看目录：有 `dist/manifests.yaml` 就是 release（预构建 JS），
   否则是 dev（服务 `src`、按需转译、热重载）。
3. **业务代码里能直接写 SQL 吗？** 能写，但表名/列名只能来自 `SchemaRegistry`（`db.table()` 构造器），
   值一律走绑定参数。裸 SQL 还有「表归属守卫」盯着跨模块访问。

> 站点内容由 `vitepress/scripts/sync-docs.mjs` 从仓库 `docs/` 与 `sample/` 同步生成，
> 每页顶部标注了生成日期与源文件。
