# PR/修复：WS 目录镜像路由与 api.ts 同源（`--module-root` 下多出一层 `api` 布局段）

> **受众**：only-js 维护者（可直接作为 issue / PR 正文）。
> **状态**：已评审（四路专家并行只读评审，2026-10-24）→ **已实现并实测**（v0.1.87，
> workspace 1449 例全绿 + module-root 真机握手冒烟，见 §2.1）。未提交，待确认后发版。
> **变更类型**：缺陷修复 + **行为变更**（仅 `--module-root` 且 `ws.ts` 不在挂载根首层时
> URL 改变；平铺布局逐字不变）。
> **下游消费方**：`@oj-module/cli`（`ojm`）——其 `api --check` 按文档口径推算 WS 路径。

---

## 1. 摘要

`--module-root`（v0.1.85）把 api 侧下沉到 `src/<模块>/api/`（`manifest.yaml` 在该层）后，
WebSocket 的目录镜像路由比 HTTP 的目录镜像路由**多一层 `api` 段**：

```
src/wiki/api/tree/api.ts  →  GET /api/wiki/tree     ✅（api 是模块根，不是路由段）
src/wiki/api/ws.ts        →  GET /api/wiki/api/ws   ❌（期望 /api/wiki/ws）
```

HTTP 侧走 `ModuleIndex::locate`（`prefix` + 模块根内路径），WS 侧走「文件父目录相对**挂载根**」
的平铺口径，两套算法由此分叉。连带后果：鉴权守卫面 `path_no_base` 同为 `/wiki/api/ws`，
按文档配的 `anonymous_paths: /wiki/ws` 命中不了 → 浏览器 WS 握手 401
（浏览器 `WebSocket` 发不了 `Authorization` 头，匿名路径是其唯一入口）；而下游
`ojm api --check` 按文档口径算的是 `/wiki/ws`，看到已在列表里就放行——**本该捕获的场景零告警**。

## 2. 实测（oj 0.1.86，module-root 布局，base 前缀 `/api`）

```text
GET /api/wiki/ws            → 404
GET /api/wiki/api/ws        → 401   ← 路由存在，但被鉴权守卫拦
GET /api/wiki/zzprobe/ws    → 404   （src/wiki/api/zzprobe/ws.ts 探针，同样 404）
```

对照实验（另一工程、同一 oj 二进制）：`/v1/api/news/ws` → 404，
`/v1/api/news/api/ws` → 握手成功。说明「多一层 api」是该布局下的系统性行为。

### 2.1 修复后实测（v0.1.87 冒烟，临时工程 `src/wiki/api/{manifest.yaml,ws.ts}`，base=`/api`）

```text
ws: 1 mount(s)
  GET /api/wiki/ws → wiki/api/ws.ts [anonymous]      # anonymous_paths 含 /wiki/ws

GET /api/wiki/ws       → 101 Switching Protocols     ✅（修复前 404）
GET /api/wiki/api/ws   → 404                         ✅（旧的错误挂载已不存在）

# 去掉 anonymous_paths 里的 /wiki/ws 后重启
ws: 1 mount(s)
  GET /api/wiki/ws → wiki/api/ws.ts [guarded]
GET /api/wiki/ws       → 401 Unauthorized            ✅（守卫面 = /wiki/ws，与配置同根）
```

## 3. 根因（代码级）

| 面 | 路径计算 | 位置 |
|---|---|---|
| api.ts | `ModuleIndex::locate` → `{prefix}/{模块根内目录}` | `serve/src/routes.rs` `mirror_rel`（原 `dev_rel`） |
| ws.ts | `file.parent().strip_prefix(root)`（root = 挂载根），无模块索引 | `serve/src/ws.rs:172-183`（原实现） |

- 模块根 = 含 `manifest.yaml` 的目录。`src/wiki/api/manifest.yaml` → 模块根是 `src/wiki/api`，
  `api` 是**布局分隔符**，本身不进 URL。
- 平铺布局（`src/news/ws.ts`，ws.ts 与模块目录同级）下两套算法结果逐字相同，故该分叉
  在 v0.1.85 之前不可见。
- `path_no_base`（守卫面，`ws.rs`）由同一处 rel 推导，随之错位。

## 4. 方案

1. **口径同源**：`routes.rs::mirror_rel` 提为 `pub(crate)`，`ws.rs` 的 WS 挂载计算改调它；
   `oj/src/app.rs` 装配时把已有的 `module_index` 传进 `mirror_routes`。新增
   `routes::join_url(base, rel)`（逐段过滤空段）供两侧共用——顺带修根前缀挂载
   （`base = /`）时的 `{base}//ws` 双斜杠路径（HTTP 侧同病一并修）。
2. **挂载清单外显**：`ws::ws_mounts() -> Vec<WsMount { path, path_no_base, file }>`，
   启动打印每条挂载（路径 + 相对挂载根的源文件 + 匿名标记），供人工与下游工具核对；
   装配与打印分离（`serve` 不打印）。
3. **碰撞 fail-fast**：口径统一后可能出现「模块内 ws.ts」与「模块外散 ws.ts」映射到同一 URL。
   同目录 `ws.ts`/`ws.js` 兄弟保持先到先得；**不同目录映射到同一 path → 拒启并点名两个文件**
   （胜者决定执行哪份 JS，属安全面；仓库先例：模块 `prefix` 撞车拒启）。
4. **release 不动**：`dist/<name>-<version>/ws.js` 仍挂 `/api/<name>-<version>/ws`
   （已文档化的 v0.2 已知限制）。dev 与 release 的 WS 分叉**登记为后续工单**（第 8 节）。

### 4.1 评审记录（2026-10-24，四路并行只读评审）

| # | 意见 | 处置 |
|---|---|---|
| 1 | 安全：碰撞只 WARN 不够，胜者由文件遍历顺序决定「谁拿到这条连接」，应 fail-fast | **采纳**（非同目录兄弟 → 拒启） |
| 2 | 工程师：axum 同 path 二次 merge 直接 panic，`seen` 去重必须保留 | 采纳（去重保留，冲突路径根本不注册） |
| 3 | 工程师：根前缀（`base = /`）拼出 `//ws` 是真实缺陷 | **采纳**（`join_url` 两侧共用） |
| 4 | 产品 + 安全：「守卫存在但未命中 anonymous_paths 就 WARN」对每条需鉴权的 WS 必报 → 告警疲劳 | **采纳并改判**：改为清单里对命中 `anonymous_paths` 的挂载标 `[anonymous]`——这才是安全评审要的「原本 401 变匿名可达」的可见面 |
| 5 | 架构师：不要把 `fs_seg_to_pattern` 带进 WS（会无声开 `_name_` 参数段口子） | 采纳（WS 段恒字面，限制表维持） |
| 6 | 架构师：挂载清单应由 `serve` **返回**而非内部 `eprintln`，让下游可消费 | 采纳（`WsMount` 三元组，含 `path_no_base`） |
| 7 | 工程师 + 安全：`walk_files` 无剪枝，`node_modules`/点目录/symlink 目录会被扫进挂载面 | **采纳**（与模块发现同口径剪枝；api.ts 与 ws.ts 共用同一 walker） |
| 8 | 安全：release 下优先 `ws.js`（dist 残留 `ws.ts` 不应抢先） | 采纳（`ws_mounts` 收 `ts` 参数） |
| 9 | 架构师：语义下沉到 `ModuleIndex::url_segments_of` 才是真源 | **不采纳**（本版最小改动；抽 `mirror_rel` 已满足同源，重构成独立工单） |
| 10 | 架构师：WS 纳入 `RouteTable` 统一打印/下游消费 | **不采纳**（WS 是 upgrade + 独占帧池，与动词/契约/openapi 语义不通） |
| 11 | 产品：`oj build` 落 ws 行进 `routes.js` 以收 dev/release 债 | **不在本版**（登记第 8 节工单） |
| 12 | 产品：dev 无文件监听，新增 ws.ts 需重启 | 采纳（写进 CHANGELOG 与限制表） |

## 5. 实施清单（跟踪）

- [x] 四路专家评审 + 处置登记（本节 4.1 / 第 9 节）
- [x] `serve/src/routes.rs`：`mirror_rel` 提 `pub(crate)`；新增 `join_url`；`walk_files` 剪枝
- [x] `serve/src/ws.rs`：`WsMount` + `ws_mounts()` + `mirror_routes(&[WsMount], ..)`；碰撞 fail-fast
- [x] `oj/src/app.rs`：传 `module_index` 与 `ts`；启动打印 WS 挂载清单（含 `[anonymous]` 标记）
- [x] 测试：module-root 挂载（`src/wiki/api/ws.ts` → `/api/wiki/ws`，`path_no_base=/wiki/ws`）、
      平铺回归、模块外散文件回落、release 版本段回归、碰撞拒启、`join_url` 根前缀、
      `node_modules` 剪枝、module-root 真实握手（serve 27 例全绿）
- [x] 文档：`docs/devkit/` 四件 + `user-manual.md` + `ops-manual.md` + `websocket.md`
- [x] CHANGELOG v0.1.87（含「行为变更（升级注意）」）+ `oj/Cargo.toml` 0.1.87 + `Cargo.lock` 版本分界
- [x] 门禁：`cargo fmt --check` 干净 / `clippy --release --workspace --all-targets -D warnings` 0 /
      `cargo test --release --workspace` 全绿（1449 passed，exit 0）/ `cargo xtask build` + `-p xtask` devkit 契约用例
- [x] 真机冒烟：module-root + `ws.ts` 握手 101、`[anonymous]`/`[guarded]` 标记（§2.1）

## 6. 兼容面（须写进 CHANGELOG）

- **平铺布局**（无 `--module-root`）：`plain_rel` 与索引命中结果逐字相同 → **URL 零变化**
  （`sample/src/news/ws.ts` 仍 `/v1/api/news/ws`）。
- **`--module-root` 且 ws.ts 在模块根内**：URL 由 `/api/wiki/api/ws` 变 `/api/wiki/ws`
  （旧 URL 404）。同时 `path_no_base` 由 `/wiki/api/ws` 变 `/wiki/ws`——
  **已在 `anonymous_paths` 里的条目语义随之改变**（原 `/wiki/api/ws` 条目失效；
  若表内有 `/wiki/*` 之类条目，原本不命中的 WS 可能变为匿名可达 → 启动清单标 `[anonymous]`）。
- **release**：不变（仍含版本段）。

## 7. 已知限制（维持）

- WS 目录镜像不做 `_name_` → `{param}` 转换（`_id_/ws.ts` 是字面段）。
- release 的 WS URL 含模块版本段（`…/news-0.1.0/ws`），与 dev 的 `…/news/ws` 不同源。
- dev 无文件监听：新增/删除 `ws.ts` 需重启进程（api.ts 走 `?v=mtime` 热重载，WS 路由只在启动构建）。

## 8. 后续工单（不在本版）

1. `oj build` 把 WS 路由落进 `routes.js`/manifest，release 侧读清单、缺失才回退 fs 镜像 ——
   收 dev/release 分叉债，并给 WS 一个显式声明位（未来支持参数段的承载点）。
2. `ModuleIndex` 下沉 `url_segments_of(file)`，让 `dev_rel` / `rel_pattern` / WS 三方同取真源。
3. 给下游一个机器可读的 WS 挂载面（`--list-ws` 或 openapi 扩展），取代「从目录结构推路径」。

## 9. 评审过程备注

- 四路评审（工程师 / 架构师 / 产品 / 安全）并行只读，共 12 条意见：采纳 8、改判 2、
  不采纳 2（不采纳项均为重构或跨版本议题，已转为第 8 节工单）。
- 「匿名未命中 WARN」是用户原始选项，经产品与安全两路独立指出必误报后改判为
  「匿名命中标记」——**改判已在本文件登记，未静默偏离**。
