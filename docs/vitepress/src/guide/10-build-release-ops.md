---
title: 10 · 构建发布与运维
updated: 2026-09-08
---

# 10 · 构建发布与运维

## 构建：`oj build`

```bash
./bin/oj build -d sample/src -o sample/dist     # 按模块产出版本目录 + 锁 + tgz
./bin/oj build --check -d sample/src            # 只做结构检查（S002–S006），不落盘
```

产物：

```
dist/
├── <module>-<version>/     版本目录：api.ts → api.js（剥离 .route、import 重写成带版本路径）
├── <module>-<version>.tgz  可分发归档
├── manifests.yaml          模块锁（release 模式的聚合依据）
├── routes.js               release 模式唯一的路由来源
└── tasks/                  tasks/ 的转译镜像（非版本化资产）
```

`--check` 是 CI 门禁：表归属双向一致（S005）、manifest 完整性等结构问题会在这里挡下，
**不落盘**。

## dev vs release

| | dev | release |
|---|---|---|
| 判定 | 目录里没有 `dist/manifests.yaml` | 有 `dist/manifests.yaml` |
| 服务内容 | `src/`（TS 源码） | `dist/`（预构建 JS） |
| 转译 | 按需转译 + mtime 缓存 + 热重载 | 不转译 |
| 路由来源 | 目录镜像 + 路由表 | `routes.js`（唯一） |
| 迁移 | `auto`（启动即应用） | `verify`（账本落后拒启） |

release 部署的正确顺序（顺序错了会被门禁拦）：

```bash
./bin/oj build   -d src -o dist
./bin/oj migrate -c config.yaml -d dist      # 必须先迁移
./bin/oj server  -c config.yaml --api-path dist
```

## 发布布局

把整个 `bin/` 拷到目标机器即可：

```
bin/oj
bin/plugins/<host-triple>/*.so|*.dll|*.dylib
bin/devkit/                     随包交付的业务开发者手册（global.d.ts + api-manual + skill）
```

插件在目标机器上的发现顺序同样是四级（`OJ_PLUGINS_DIR` > `plugins_dir` > `<exe>/plugins`
> `<workspace_root>/bin/plugins`），所以**保持目录结构不动**就能零配置被发现。

## 运维要点

| 项 | 说明 |
|---|---|
| 证书 | 必配、无逃生口。临期用 `oj-cert renew` 重签；过期且过宽限期 → 所有 GET 被 403 |
| 日志 | 默认只落盘（`server.logs_dir`，按天 + 按大小滚动）；终端输出要 `--console-log` |
| 日志级别 | 由 `RUST_LOG` 环境变量控制，配置文件里配不了 |
| 静态站点 | `server.app_path` 指向目录，API 未命中的 GET/HEAD 落静态，带路径穿越防护 |
| 上传 | axum 硬顶 + 信封 413 双上限 |
| 插件 | `GET {base}/plugins` 可查已加载插件与宿主 ABI；`plugins()` 全局同源 |
| 对账 | `oj schema diff` 声明 vs 实库，漂移 exit 1，建议进 CI |

## 排障顺序

服务起不来时，按这个顺序排查最省时间：

1. **证书**：两个路径都配了吗？过期了吗？（缺证书是拒绝启动，不是启动后报错）
2. **插件**：`bin/plugins/<triple>/` 里有 cdylib 吗？ABI 版本匹配吗？（加载失败会在启动日志里点名）
3. **迁移**：release 模式账本落后会拒启，先 `oj migrate`。
4. **端口/权限**：`db` 的 sqlite 文件、blob 的 `root` 目录是否可写。
5. 还不行就开 `RUST_LOG=oj=debug --console-log` 看启动日志。

详细的症状 → 原因 → 处置表见[12 · 排障 FAQ](./12-faq.md) 与[运维手册](../reference/ops-manual.md)。

## 延伸

- [用户手册](../reference/user-manual/index.md)（命令与 config 全字段）
- [运维手册](../reference/ops-manual.md)
- [04 · CLI 与装配](../modules/04-oj-cli.md)
