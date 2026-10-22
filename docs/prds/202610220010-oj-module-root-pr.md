# PR：`oj build` 支持可配置模块根（`--module-root`），模块身份与别名按 `manifest.name` 解析

> **受众**：only-js 维护者（可直接作为 issue / PR 正文）。
> **状态**：待上游评估。现状限制已在 `oj 0.1.84`（macOS arm64）复现。
> **变更类型**：新增能力（opt-in），默认行为不变。
> **下游消费方**：`@oj-module/cli`（`ojm`）。

---

## 1. 摘要

`oj` 目前把「`-d` 的首层子目录」当作模块目录，且模块身份 = 目录名。这使「一个模块一个目录」的工程布局无法落地：

```
src/<name>/api/manifest.yaml     # 模块根在第二层，oj 看不见
```

请求四项能力（均为 opt-in）：

1. `--module-root <glob>`：模块根相对 `-d` 的匹配模式，缺省等价今天的一层扫描；
2. 模块身份与 `#/m/x` 别名首段按 `manifest.name` 解析，目录名不再参与；
3. 同一套发现逻辑覆盖所有按目录扫描模块的子命令；
4. 发现 0 个模块时非零退出（当前为静默成功）。

目标：下游可把前后端源码收敛为 `src/<name>/{web,api}`，同时 **`-d src` 的既有行为逐字节不变**。

## 2. 动机

`ojm` 工程当前一个模块横跨 `api/src/<name>` 与 `web/src/<name>` 两棵树，另有仓库根的方案清单与模块登记表。收敛到 `src/<name>/{web,api}` 后：模块可整体搬仓/拆仓；模块清单可由扫描得出；打包不再需要「先构全站再挑自己」的事后过滤（该过滤曾导致宿主前端被覆盖进方案包）。

上游若不支持，下游只能退回「构建前把源码物化到临时目录」的适配器——多一次整树拷贝、dev 时序复杂、symlink 语义需自行摸索。这是本 PR 的动机。

## 3. 现状（实测）

```text
$ oj build --help
Usage: oj build [OPTIONS] [MODULE]
Arguments:
  [MODULE]  目标模块名（src 首层子目录）；省略 = 全部模块
Options:
  -d, --dir <DIR>  源码目录 [default: src]
  -o, --out <OUT>  产物目录 [default: dist]
```

两种失败形态（`oj 0.1.84`，均非零退出）：

| 输入 | 输出 |
|---|---|
| 旧布局 `src/m1/{manifest.yaml,api.js}` + `-d src` | `oj build: m1 v0.1.0 → out/m1-0.1.0` ✓ |
| 把模块目录本身当 `-d`（`-d src/m1`） | `oj build: 0 module(s)`，产物目录不生成 |
| 目标布局 `src/m1/api/manifest.yaml` + `-d src` | `oj build: module 'm1' missing manifest.yaml` |

第三种是本次诉求的常态：oj 在 `src/m1/` 找 manifest，而它在 `src/m1/api/`。

另外两项现状约束（影响方案形状）：

- **跨模块引用在产物里被实化成版本目录相对路径**。下游产物实测 `../_platform-0.1.0/` 出现 39 处、`../notification-0.1.0/` 1 处（手册 §5：按 `dist/manifests.yaml` 锁钉版本）。因此所有模块必须共用**同一个 `-o`**，产物保持 `<name>-<version>/` 兄弟布局。
- **`#/m/x` 别名首段是模块目录名**（手册 §5）。若模块根目录统一叫 `api`，所有模块的别名首段都变成 `api`。

## 4. 最小复现

```bash
set -e
T=$(mktemp -d /tmp/oj-module-root-XXXX); cd "$T"

# 目标布局：模块根在第二层
mkdir -p src/m1/api
printf 'name: m1\nversion: 0.1.0\ndesc: probe\n' > src/m1/api/manifest.yaml
printf 'export async function ping() { return { ok: true } }\n' > src/m1/api/api.js

oj build -d src -o out            # 现状
# 期望：发现 m1 并产出 out/m1-0.1.0/
# 实际：oj build: module 'm1' missing manifest.yaml（非零退出）
```

对照（旧布局，证明问题只在层级）：

```bash
mkdir -p flat/m1
printf 'name: m1\nversion: 0.1.0\ndesc: probe\n' > flat/m1/manifest.yaml
printf 'export async function ping() { return { ok: true } }\n' > flat/m1/api.js
oj build -d flat -o out2          # → out2/m1-0.1.0/ + out2/m1-0.1.0.tgz + out2/manifests.yaml
```

## 5. 提议

### 5.1 参数

| 参数 | 语义 | 缺省 |
|---|---|---|
| `--module-root <glob>` | 模块根相对 `-d` 的匹配模式；命中目录 = 一个模块根 | 等价 `*`（首层） |

约束：只接受相对模式，禁止 `..` 越出 `-d`；命中 0 个 → 非零退出并列 `-d`；命中目录缺 `manifest.yaml` → 非零退出并列目录。

### 5.2 模块身份与别名

- 模块 `name` 一律取 `manifest.yaml.name`。
- 使用 `--module-root` 时，目录名仅作定位，不参与 `name == 目录名` 校验；不使用时保持现状校验。
- `oj build <MODULE>` 的位置参数按 `name` 匹配。
- `#x` 仍锚在本模块根（向上找最近的 `manifest.yaml` 祖先）；`#/m/x` 的首段 `m` 按 **`manifest.name`** 解析（现状是目录名）。

### 5.3 子命令一致性

同一 resolver 必须覆盖所有按目录扫描模块的子命令，否则下游会出现「构建通过、对账失败」：

| 子命令 | 现状 | 不同步的具体后果 |
|---|---|---|
| `oj build` | `-d` 首层 | — |
| `oj serve --api-path <源码树>`（dev） | 首层 | dev 加载 0 模块，`/health` 仍 200 → 假绿 |
| `oj migrate -d` | 首层 | 迁移不执行，退出 0 |
| `oj schema` / `oj schema diff` | 首层 | 0 模块 → CI 假绿 |
| `oj test` | 首层 | 0 用例，退出 0 |
| `oj openapi` | 首层 | 生成空 spec |
| `oj exec` | 首层 | 找不到模块 |

建议抽 `dir + pattern → [moduleRoot]` 的共享 resolver，逐子命令接同一入口并各补一条回归。

### 5.4 产物

- 默认仍写 `<name>-<version>/` + `manifests.yaml` + `<name>-<version>.tgz`，位置由 `-o` 决定，**不变**。
- 不新增外层布局；`{web,api}` 归拢由下游负责，oj 不需要知道前端。

## 6. 兼容性承诺与建议修正

| # | 承诺 |
|---|---|
| A | 不带 `--module-root` 时，发现、身份校验、产物字节与当前版本一致 |
| B | `manifests.yaml` 的 name→version 锁、多版本共存、`gc` 语义不变 |
| C | 新增参数 opt-in，旧命令行零改动 |
| D | 建议修正（可拆独立 PR）：发现 0 个模块时退出非零 |
| E | 建议修正（可拆独立 PR）：`--module-root` 命中目录缺 manifest 时列全量目录 |

## 7. 验收用例

1. 旧布局 `-d src` 产物与 main 分支 **sha256 一致**（oj 为确定性构建）。
2. `--module-root '*/api'` 的发现数 == `find src -maxdepth 2 -name manifest.yaml | wc -l`。
3. 目录名与 `manifest.name` 不一致时：带 `--module-root` 通过；不带时报原错误。
4. `#/m/x` 在 `m != 目录名` 时可解析；缺 `deps` 声明仍报 S008。
5. `oj serve --api-path <src> --module-root '*/api'` dev 模式可加载全部模块（release 模式仍按 `manifests.yaml`）。
6. `--module-root '*/api'` 命中 0 个 → 非零退出 + 可读文案。
7. `oj build m1 --module-root '*/api'` 只构建 m1，且 `manifests.yaml` 保留其他模块条目。

## 8. 备选方案

| 方案 | 说明 | 取舍 |
|---|---|---|
| **A（本 PR）** | `--module-root` + name 权威 | 参数恒定，dev 可用，改动集中在一个 resolver |
| B | 显式映射 `--module <name>=<dir>` | 参数随模块数线性增长；增删模块需重启 dev |
| C | 下游构建前物化（`.ojm-stage/`） | 不动 oj，但每次多一次整树拷贝，symlink 语义需实测，dev 时序复杂 |

若 A 短期无法排期，B 可过渡，但**子命令一致性（§5.3）与别名命名空间（§5.2）不能让步**——否则问题从「oj 不认布局」退化为「oj 认一半」。

## 9. 影响面

- **上游**：新增参数与共享 resolver；默认路径不变，现有用户无感知。
- **下游 `ojm`**：
  - `buildBackend` 传 `--module-root '*/api'`，**单一 `-o`** 构建全部模块（保持 `<name>-<version>/` 兄弟布局与 `manifests.yaml` 单一锁）；
  - config 上移到仓库根后，`oj` 的 project root 变为仓库根（该改动由下游自行承担）；
  - dev 用 `--api-path src --module-root '*/api'`；
  - 打包源、安装链路（包内布局、版本锁、`modules.json`）**零改动**。
- **文档**：devkit 手册「交付（release）跑法」与 `oj build` 参数表补一段「模块根不在首层」的用法；§5「导入解析」补 `#/m/x` 按 name 的说明。
