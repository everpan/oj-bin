# oj build 子命令设计（按模块构建 + 版本目录 + tgz）

日期：2026-08-23。需求来源 `docs/cli2.md` build 一节；多角色评审（工具链/运行时/DX/安全）整合稿。

## 0. 已确认决策

| 决策点 | 结论 |
|---|---|
| 构建引擎 | 扩展现有自研转译管线（swc/deno_core），**不引入 vite/turbopack**（后者无可用的稳定 Rust 嵌入形态，经 Node 用与 vite 同代价；bare import 运行时沿 node_modules 解析，无构建期打包需求） |
| 产物布局 | 每模块版本目录 `dist/<module>-<version>/`，模块内自带 `routes.js` |
| 版本锁定 | 多版本目录共存；`dist/manifests.yaml`（`user: 0.1.0` 映射）是部署侧锁定文件，server 按它加载；build 时更新本模块条目 |
| pattern 与 base | routes.js 的 pattern **不含 base**（tgz 与部署路径解耦），server 聚合时统一拼前缀 |
| 哈希命名 | `api.ts` 产物 = `api-<sha256[:16]>.js`（内容哈希取 16 hex，cli2.md 原意） |
| 兼容性 | **破坏性变更**：替换现有全量 `dist/` 镜像 + 全局 `dist/routes.js` 机制；e2e 与 sample 同步迁移 |

## 1. CLI

```
oj build [module] [-d src] [-o dist]
```

- 有 `module` → 单模块构建；无 → 全量（`manifest::load_modules` 遍历，解决改公共 `_shared` 后逐模块重敲命令的问题）
- **`-b` 从 build 删除**（pattern 无 base 后无用途）；`-d` 语义保持"源码目录"（server 的 `-d` 是服务目录，各自文档写清）
- `BuildArgs` 增加 `module: Option<String>`（args.rs 手写解析风格不变）

## 2. 构建流程（改造 `oj/src/build_cmd.rs`）

1. **入口校验**：`module` 白名单——非空、`[A-Za-z0-9_-]`，禁 `/` `\` `\0` `..`（module 进路径拼接，信任边界）。
2. **manifest 校验**：`src/<module>/manifest.yaml` 存在、`name == module`（现有强约束）；`version` 白名单——非空、`[A-Za-z0-9.-]`、拒连续点（兼容 `0.1.0-beta`；version 进目录名与 tgz 名，信任边界）。
3. **清场**：`dist/<m>-<v>/` 已存在 → `remove_dir_all` 后重建（同版本重建的旧哈希残留根治）。
4. **转译落盘**（模块内递归遍历，确定性排序）：
   - `api.ts` → `cached_transpile` + `strip_route_decls` + `fix_relative_imports` → 产物内容 SHA-256 前 16 hex → `api-<h>.js`
   - 其余 `.ts` → 转译 + `fix_relative_imports`（**不剥** `.route`）→ 原路径同名 `.js`
   - `manifest.yaml` → 原样复制
5. **import 守卫**：模块内任何 `.ts` 的相对 import 解析目标为 `api.ts` → 构建失败并列出（api.ts 只许作路由入口——哈希改名的配套防线）。
6. **模块 routes.js**：内省复用 `build_table`（内存 sqlite、bridge_introspector），改传空 base 得相对 pattern；行变换后写 `dist/<m>-<v>/routes.js`：
   - `pattern`：**无首斜杠、无 base，含模块名段**，规则见 §2.1；
   - `file`：相对版本目录根的哈希文件名（`api-<h>.js`），强制正斜杠；
   - 头注释"由 oj build 生成；勿手改"。
7. **锁文件**：`dist/manifests.yaml` 原子更新（写 tmp + rename）：置 `<module>: <version>`，保留其他模块条目。多进程并发构建的读-改-写 race 不做锁（ponytail ceiling：单机常规场景原子写已覆盖）。
8. **tgz**：`dist/<m>-<v>/` → `dist/<m>-<v>.tgz`（tar+gzip）。entry 元数据抹平：mtime=0、mode 0644/0755、无 uid/gid → 确定性制品；解压前缀即 `<m>-<v>/`。

### 2.1 routes.js 条目规范（含模块名段）

内省建表的 pattern 本就含模块目录段（`build_table` 以 src 根为 root）。相对化 = 剥 base 前缀、去首斜杠：

```
{ method: "get", pattern: "user/profile/detail/{id}", file: "api-a1b2c3d4e5f60718.js" }
```

- `.route="/{id}"`（相对声明）→ `user/profile/detail/{id}`
- `.route` 以 `/` 开头（根级声明）→ `v2/user/{id}`（剥首斜杠）
- 模块根 `src/user/api.ts` 无 `.route` → `user`

## 3. server release 装配（改 `oj/src/server_cmd.rs` ts=false 分支）

```
读 dist/manifests.yaml ──缺失/空/非法→ fail-fast（"run oj build first"）
逐 (module, version)：
  module/version 白名单（同 §2 第 1/2 条）→ dist/<m>-<v>/ 存在？
  → manifest.yaml name == module？→ bridge_default_reader 读 routes.js
  拼接：pattern_full = "/" + trim(base) + "/" + pattern（pattern 已无首斜杠；
        拼后拒绝空段；空 pattern 不会出现——最少含模块名段）
  file 前缀：<m>-<v>/<file>（file 来自模块内，正斜杠）
全模块 entries 扁平化 → 单次 RouteTable::from_entries(root=dist)
```

- **跨模块冲突**：扁平化单次 `from_entries` → 冲突/合并语义复用现有 `register`（多次分别 insert 会让 matchit 直接报错而非 Conflict 哨兵）。
- **from_entries 内加 file 守卫（根因位置）**：`root.join` 前逐行校验 `e.file`——空段/`.`/`..`/`\`/`\0` → 进 failures 丢弃。任何未来消费者自动受保护。
- **pattern 守卫**：拼 base 后含空段（`//`）→ 进 failures。
- 现有行为保持：目录镜像兜底仅 dev（`lib.rs` `fallback: ts.then(...)`），release 表外 404。
- 逐模块串行加载（每模块一个线程 + current_thread runtime）：仅启动期，模块个位数，标 ceiling 不优化。
- failures 非空 → **fail-fast**（dev 保持告警跳过；release 制品必须干净）。

## 4. 错误决策表

| 场景 | build | server release |
|---|---|---|
| module 参数非法字符 / 模块不存在 / manifest 缺失或 name 不符 | fail-fast | — |
| version 非法字符 | fail-fast | fail-fast |
| 相对 import 指向 api.ts | fail-fast | — |
| manifests.yaml 缺失/空/指向不存在版本 | — | fail-fast |
| 模块 routes.js 解析失败 | — | fail-fast |
| file/pattern 字段非法（from_entries 守卫） | — | 行丢弃进 failures → fail-fast |
| 聚合后冲突/非法 pattern | 内省侧检出告警 | fail-fast |

## 5. 依赖

新增三个纯 Rust crate：`tar`、`flate2`（默认 rust backend）、`sha2`。工作区唯一新增。

## 6. 测试

- 单测：module/version 白名单拒绝；哈希稳定性（同输入两次构建同哈希）；import 守卫拒绝；manifests.yaml 合并保留他模块条目；from_entries file 守卫；pattern 拼接（根级/`/`开头声明/普通）；同版本重建清场。
- e2e 迁移：现有 `dist/routes.js` 断言改 manifests.yaml + 版本目录；补 build → server release 全链路（构建后起 server 打请求验证）。
- sample 迁移：`sample/dist` 重建为新布局。

## 7. 显式不做（YAGNI）与升级路径

- vite/turbopack/esbuild 构建期打包——若将来需要"npm 依赖打进 tgz 自包含发布"，评估 esbuild 进程调用或 rolldown。
- 多进程构建文件锁（manifests.yaml 原子写覆盖单机场景）。
- tgz 签名/SHA256SUMS 清单（v0.2；当前威胁模型=可信内网分发通道）。
- 旧版本目录自动 GC（manifests.yaml 不指向即死数据，手删）。
- 哈希 16 hex 碰撞：本地可信构建威胁模型下忽略。

## 8. 文档同步（实现期一并完成）

`docs/cli2.md`：删 vite 表述（server 节末尾、build 节）；`ap-[hash]` 拼写统一为 `api-[hash]`；routes.js 位置=模块版本目录内、pattern 规范=无首斜杠无 base；补 manifests.yaml 语义与 release 装配规则；`-b` 不再是 build 参数。
