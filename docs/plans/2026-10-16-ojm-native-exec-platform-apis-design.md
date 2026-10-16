# ojm 后端原生化：exec 地基与平台 API 设计稿（v2，评审后修订）

- 日期：2026-10-16（v2 同日修订）
- 状态：四路专家评审（架构师/工程师/产品/安全）意见已处置，处置表见 §9
- 范围：oj-bin 仓库 6 个 PR（OJ-1 ~ OJ-6）的整体设计
- 约束来源：用户逐段确认的决策 + 仓库既有红线（CLAUDE.md）
- **范围声明**：本设计交付 oj 平台侧原语与 CLI 改形；安装编排模块
  （ojm-installer：fetch 下载 + 校验 + 落位 + 回写 + 触发的 JS 编排）属 oj-module
  仓库，另行设计。本稿只保证原语面齐备且安全。

---

## 0. 背景与目标

解决方案市场（oj-module）安装链路「下载 → 校验 → 解压落位 → 迁移 → 回写配置 → 热更」
目前依赖 Node 侧 `ojm install` 子进程。目标：让 oj 后端 JS 原生完成安装闭环
（M6「后端原生化」），拆解为 6 个 PR：

| PR | 优先级 | 一句话 |
|----|--------|--------|
| PR-1 (OJ-4) | P0 | crypto 新增 Ed25519 验签（+ sha256Hex 扩二进制） |
| PR-2 (OJ-1) | P0 | 归档解压（本设计改形为：exec 外部命令能力，argv 模板化） |
| PR-3 (OJ-2) | P1 | 配置热更进程内触发 `server.reloadConfig()` |
| PR-4 (OJ-6) | P1 | 迁移执行内部化（改形为：CLI 增强 + exec 模板调用） |
| PR-5 (OJ-3) | P2 | YAML 读写原语（parse + stringify，带解析预算） |
| PR-6 (OJ-5) | P3 | 远程 api mount（远期，只记方向） |

安装闭环逐环节归属：下载=既有 `fetch`（支持 `arrayBuffer()`）；sha256=PR-1 扩面；
验签=PR-1；解压落位=PR-2（exec 模板 + 后置校验 + §1.5 原子落位次序）；
manifest 解析与锁写回=PR-5（stringify 只用于机器产物 manifests.yaml）；
config.yaml 回写=§1.6（trusted_paths + 文本手术三步模式）；迁移=PR-4；
热更触发=PR-3。

### 关键用户决策（已确认，不再重开）

1. 解压**不走内建**：给 oj 增加「执行外部命令」的原生能力，用系统 `tar` 完成解压。
2. exec 安全形态 = **配置白名单 + argv 模板 + 平台特权限定**（否决完整 shell）。
3. 特权认定 = **config 白名单**（否决命名约定、否决「签名即特权」）。
4. 配置布局 = **platform 段统管**：`platform.modules` 管平台 API 特权；
   `platform.exec` = 模块 × 命令矩阵；`platform.trusted_paths` 管路径授权面。
5. PR-4 = **进程内 migrate.run 否决**（用户主动改形）：复用 exec 调 `oj migrate`，
   bridge 层零新增 op；代价是 CLI 三处改造 + exec 模板机制。

---

## 1. 地基：`platform` 配置段 + `exec` 全局

### 1.1 配置面（`src/config.rs` 新增，与 `fs:` 段同级）

```yaml
platform:
  modules: ["ojm-installer"]        # 平台特权模块：reloadConfig 等平台 API 调用资格
  trusted_paths: ["./config.yaml", "./dist"]   # 额外授权路径根（缺省 = [fs.root]）
  exec:                             # 模块名 → 命令授权；列出该模块 = 授权其 exec
    ojm-installer:
      allow:                        # argv 模板：literal token 定死，占位符吸收一个 token
        - "tar -xzf ${path:archive} -C ${path:dest}"
        - "tar -tzf ${path:archive}"
        - "oj migrate -c ${path:config} --format json -d ${path:root} ${raw:module}"
      env: ["OJ_SECRET_KEY"]        # 透传环境变量白名单（值不进 JS）
```

- 两名单**独立**：可「有 API 特权无 exec」或反之；缺省省略 = 全 deny（fail-closed，
  对齐 `fs:` 未配置即 deny 的既有惯例）。
- **allow 条目 = argv 模板**（安全评审阻塞项 1 的修复）：不再存在「白名单命令 + 自由
  argv」形态——tar 的 `--use-compress-program` / `--to-command` / `--checkpoint-action`
  / 任意 `-f` 均无从注入，因为模板 literal token 必须逐字相等，JS 只能填占位符。
  - `${path:NAME}`：吸收一个 token，值按**路径**校验——相对路径以 cwd（fs.root）折叠 +
    canonicalize（复用 `join_jailed`/`resolve_existing`，`src/bridge/fs.rs:107-115`），
    解析结果必须落在 `fs.root ∪ trusted_paths` 内，否则 NotCapable；拒绝绝对路径直接
    传入（绝对路径须恰为 trusted_paths 内 canonical 结果）。
  - `${raw:NAME}`：吸收一个 token，仅做字面收集（不得含路径分隔符起步的绝对形态、
    不得以 `-` 开头——防 option 注入；`module` 名形态由调用方保证）。
  - 模板首 token（命令名）宿主侧解析：`"oj"` 特判解析为 `std::env::current_exe()`
    （serve 进程即 bin/oj，同版本自保证）；其余裸名在**父进程**用 PATH 解析为绝对路径
    后 spawn（env 空表下子进程 execvp 会回落 confstr 默认路径，行为不可控——宿主侧
    解析消除该不确定性；解析失败 = 抛错点名）。
  - 同一 argv 命中多个模板 → 报错（歧义拒绝，不静默择一）。
- `env` 白名单：显式列出的环境变量**值**透传给子进程但不进 JS 面；解决 CLI 子进程读
  `ENC[...]` 密封 DSN 的解封钥匙问题（`OJ_SECRET_KEY`）。**PATH 不继承**（宿主侧已
  解析为绝对路径，无需子进程再查）。

### 1.2 装配与裁决

- 装配期（`oj/src/serve_cmd.rs`）解析为 `PlatformGrant`（modules 集合 + trusted_paths
  canonical 列表）与 `ExecGrant`（module → 模板表 + env 白名单），冻结进 `StableState`
  （对齐 `FsGrant` 惯例：跨请求只读、不可变）。
- 平台 API 裁决收敛为**一个**函数：`assert_platform(state) -> Result<(), JsErrorBox>`，
  从 `ReqState.module` 查 `platform.modules`；不在名单 →
  `NotCapable: module 'x' is not platform-privileged`。
- **特权语义（写死进设计与 api-manual 限制表）**：
  - 特权是**请求级归属**（`ReqState.module` 由 `run_module_validated` 按模块目录祖先
    命中注入，`src/bridge/mod.rs:1250-1259`；URL/JS 侧不可伪造），**不随 import
    收窄**：特权模块 import 并执行的任何代码（含第三方库）共享其全部特权；反向安全
    （非特权模块 import 特权模块导出的函数不提权——裁决在 op 层）。
  - **tasks 与 WS 上下文无归属**（`task_connect`/`ws_connect` 以 `module=None`
    checkout）→ 平台 API 与 exec 恒 NotCapable。若安装器需要任务化触发，须另行改造
    任务路径的模块归属推导（独立改造项，不默认可用）。
  - **抢注防护**：模块名来自第三方 bundle manifest，按名锚定可被抢注。装配期校验：
    `platform.modules` 列出的模块，其物理目录必须位于**平台受控目录**（config/mount
    解析的市场安装树之外）；特权名单中的名字出现在安装树内 → 装配 fail-fast。
    api-manual 写明：「安装名为 X 的 bundle = 继承 X 特权」被装配校验阻断。

### 1.3 `exec` 全局（`src/bridge/exec.rs` 新文件）

```ts
exec.run(argv: string[], opts?: {
  timeoutMs?: number,      // 缺省 30_000，硬上限 600_000（大 DDL 迁移须显式调大）
  stdin?: string,          // 可选，一次性写入
}): Promise<{ code: number; stdout: string; stderr: string }>
```

语义要点：

1. argv 逐 token 对模板匹配（§1.1）；未授权 / 歧义 / 占位符校验失败 → NotCapable 点名。
2. **cwd = fs.root**（工作目录钉定，**非隔离边界**——措辞降级，子进程无 OS 级 chroot，
   文件可达性由模板占位符的路径校验承载）；未配 `fs:` → exec 不可用。
3. 环境变量默认空表，仅 `env` 白名单放行（§1.1）。
4. stdout/stderr **流式读取边计数**（`wait_with_output` 是无界缓冲，不可用），合计
   10MB 超限 → kill + 抛错点名（已收部分不随错返回）。
5. 超时 → 先 `kill(-pgid, SIGKILL)` 再 `child.kill()` 兜底；`cfg(unix)` 门控
   `Command::process_group(0)`；Windows 无进程组对应物，仅杀直接子进程并在部署文档
   声明。措辞不宣称「防孤儿」强保证。
6. **返回 vs 抛错边界**：进程正常跑到退出（含非 0 退出码）= **返回**
   `{code, stdout, stderr}`；执行环境失败（未授权/超时/输出超限/spawn 失败/模板歧义）
   = **抛错点名**。信号终止映射 `code = 128 + signal`。stdout/stderr 强制 UTF-8 解码，
   失败抛错（二进制输出不在本原语范围）。
7. 测试（对抗性探测用例钉死）：模板 literal 绕过尝试（token 增删替换、占位符吸收
   两个 token、`raw` 占位符夹 `-` 开头 token、路径占位符绝对/越界/TOCTOU 形态）、
   `oj` 之外命令名夹路径分隔符、非特权击穿、超时、输出上限、env 白名单外变量不可见。

### 1.4 tar 解压组合（模板 + 后置校验）

- 解压：`exec.run(["tar","-xzf","<archive>","-C","<dest>"])`（模板 literal 定死，
  两路径占位符受 §1.1 校验）。
- **安全判定后置**（安全评审意见 7 修复）：`tar -tzf` 预检降级为快速失败优化；
  **权威校验 = 解压后对 dest 全量 walk**：零容忍 symlink/hardlink/设备/FIFO；逐成员名
  复检（组件级拒 `..`、绝对路径、控制字符——不做子串匹配，`a..b` 合法）；总体积对账
  （解压结果上限，防归档炸弹填盘）。任一违规 → 删除 dest 整目录 + 抛错（不部分落盘）。
  该序声明为**共享 jail 残余风险**：同 jail 其他模块可在预检与解压间置换归档，靠后置
  校验把最终态收敛为安全。tar 方言差异（macOS bsdtar / Linux GNU）被后置校验天然
  覆盖，`xtask smoke` 探测 `tar --version` 记录环境。
- 运行环境无 `tar` → exec.run 抛错点名（部署文档写明依赖）。

### 1.5 原子落位次序（产品评审意见 3 补）

1. 解压到 `<dist>/.staging/<name>-<version>-<rand>/`（jail/受控路径内）；
2. walk 校验（§1.4）；
3. 目标 `<dist>/<name>-<version>/` 已存在 → 先 rename 至 `<...>.old-<rand>`；
4. staging rename 至目标（同盘 rename 原子）；
5. 成功后删除 `.old-*`；任一步失败 → staging/目标回滚原状，残留由下次安装回收。
   destDir 命名与 manifests.yaml 的 `<module>-<version>` 约定一致。

### 1.6 config.yaml 回写（产品评审阻塞项 1 补）

- **缺口**：config.yaml 位于部署目录，`fs.root` 缺省 `./data`——jail 外，现有 fs 轴
  够不到。修复：`platform.trusted_paths`（§1.1）同时扩 **fs 轴**与 exec 路径占位符的
  授权根（仅特权模块：fs 裁决按模块名双查）。
- **回写三步模式**（api-manual 场景化）：① `fs.readTextFile(config.yaml)` →
  `yaml.parse` 读语义；② **文本手术**（只改目标段，保注释/空白）；
  ③ `yaml.parse` 语义复检 + sha256 自校验（改前后语义 diff 即预期清单）→ 写回 →
  `server.reloadConfig()`。注释/空白变更不触发热更是既有语义（E4 实测），手术式
  编辑天然利用之。
- `platform.patchConfig(patch)` 服务端受控原语列为**远期演进**（本轮不做）；
  `yaml.stringify` 不用于 config.yaml（serde_yaml 往返丢注释）。

---

## 2. PR-1：`crypto.verifyEd25519` 验签原语

### 2.1 依赖策略（唯一 spike 项）

`ring 0.17.14` 经 `jsonwebtoken 9.3.1` 引入（**非 rustls**，评审更正），根 crate 直接
依赖链。动工第一步：`cargo tree` 实证可达性与版本；确认后根 crate 声明
`ring = "0.17"` 直依（同版本零新增编译单元）。API：
`ring::signature::UnparsedPublicKey::new(&ED25519, pk).verify(msg, &sig)`，无 feature
开关，验签微秒级 → **同步 op**（对照 `op_sha256_hex` 先例）。spike 结论写阶段小结。

### 2.2 API

```ts
crypto.verifyEd25519(publicKey: string, msg: string, sigBase64: string): boolean
// publicKey: hex/base64 编码的 32 字节原始公钥（复用 decode_key；不支持 PEM/SPKI）
// msg: UTF-8 字符串（规范串 "<name>@<version>:<sha256>"；不支持二进制）
// sigBase64: 仅 base64（STANDARD/NO_PAD；base64url 不支持——文档点名）
// 返回：验签通过 true；签名/密钥不匹配 false
// 抛错：base64 非法、公钥 ≠ 32 字节、sig 解码后 ≠ 64 字节（格式错误点名报错）
```

「**格式错 = 抛错，验签不过 = false**」双层语义。测试钉死：hex 判定优先于 base64
的次序；base64url 公钥报错文案点名。

### 2.3 连带缺口

`crypto.sha256Hex(data: string | Uint8Array | ArrayBuffer)`（bootstrap.js 按类型分流；
`ArrayBuffer → new Uint8Array(ab)` 归一后走 `#[buffer] JsBuffer` op——裸 ArrayBuffer
不是 View，先例 `src/bridge/blob.rs:546`）。

### 2.4 测试与文档

RFC 8032 向量 roundtrip；篡改 → false；错公钥 → false；非法 base64 / 公钥 31、33
字节 / sig 63 字节 → 点名抛错。文档落点：devkit 四件 + `sample/global.d.ts` JSDoc +
api-manual **错误/限制表三条**（sig 仅 base64 / msg 仅 string / 公钥须 32 字节原始
key 非 PEM）。

---

## 3. PR-3：`server.reloadConfig()` 进程内热更（评审后重构）

```ts
server.reloadConfig(): Promise<{
  generation: number; unchanged: boolean;
  applied: string[]; restart_pending: string[]; kept_listener: string[];
}>
// unchanged=true 时三清单为空数组（形状恒定，JS 无需分支）
```

- **类型下沉**（架构评审阻塞项 1 修复）：`ReloadOutcome` 定义**下沉根 crate**
  （`src/bridge/` 内，四字段 + unchanged），serve 的 `ReloadOutcome` 改为 re-export /
  `From` 转换——根 crate 不可反向依赖 serve（`cargo tree` 单向：serve → only_js），
  HTTP 端点序列化与 JS 返回共用同一结构，真源唯一。
- **注入走槽位后填**（架构意见 2 / 工程意见 1 修复）：Extras 放共享可填充槽
  `Arc<OnceLock<ReloadExecutorLike>>`（槽指针构造期冻结、内容后填，不触
  `Arc::get_mut` 红线）；`App::from_config` 穿参至 actor 工厂闭包；ReloadState 构造后
  set——照搬 `ReloadState::set_executor` 的既有解环惯例（`serve/src/reload.rs:137`）。
  每代热更新建 App 时同样带上槽指针，避免热更一次后回调失效。
- **串行化与阻塞语义**（架构意见 3 修复）：共享入口收在 ReloadState 层，复用既有
  `reload_lock`（try_lock → 撞锁返回明确 busy 错误，不静默并发）；executor 是同步
  闭包（独立线程 + 多线 runtime，`current_thread` 直接调会死锁），op 侧经
  `tokio::task::spawn_blocking` 调用（executor `Send + Sync` 已成立）。**既定语义**：
  触发 reload 的 handler 所在 actor 在热更期间被占用（秒级，DB verify + 转译）。
- **段分类扩面（安全阻塞项 3 修复，列为显式验收项）**：`diff_configs`
  （`oj/src/serve_cmd.rs:511-518`）现行 `_ => applied` 使 `platform:`/`fs:` 段变更
  热生效 → 特权模块可热改自己的白名单（自提权链）。修复：`platform`、`fs` 归入
  **restart_pending** 臂；对抗测试钉死「改 platform 段 + reloadConfig → 必须不热生效，
  须重启」。HTTP 端点处理体抽共享函数，两边同源，逐字对齐由结构保证。

测试：改 config（auth/vars 段）特权触发 → 三清单正确；platform/fs 段 diff →
restart_pending；非特权 → NotCapable；unchanged → generation 不动 + unchanged=true；
并发触发撞锁报 busy；回调槽缺席（`oj test`）→ reload not available。

---

## 4. PR-4（改形）：`oj migrate` CLI 增强 + 经 exec 模板调用

### 4.1 CLI 改造（`oj/src/migrate_cmd.rs` / `args.rs`）

1. **定向 `<module>-<version>` 目录**：positional 目前按**裸模块名**匹配
   （`args.rs:111-123`、`migrate_cmd.rs:59-64`），版本目录定向是**新增匹配逻辑**
   （按锁解析版本目录），非雏形补齐。校验：目录存在、manifest 归属匹配。
2. **`--format json`**：`run_migrate` 现为 fail-fast（首败即 Err）→ 改
   **continue-and-collect** + 聚合退出码（任一模块失败 → 非 0，json 仍完整输出）；
   `apply_module` 返回值从 `usize` 改为已应用迁移列表
   （`applied_migrations()` 调用点可得，`migrate.rs:389-408`）。
3. **错误脱敏**：json error 字段不得含 DSN/凭据（连接错误归因为方言+主机摘要）。
4. stdout 纯净性已成立（tracing 订阅器写 stderr，`oj/src/main.rs:15`）。

### 4.2 exec 配套

模板定死参数：`"oj migrate -c ${path:config} --format json -d ${path:root} ${raw:module}"`
——`-c` 受路径校验（trusted_paths 含 config.yaml），**`--baseline` / `--db` 不入模板
即不可传**（literal 必须逐字相等，安全意见 9 修复）；`OJ_SECRET_KEY` 经 env 白名单
透传。安装器（platform.modules 特权）直接调 exec.run。

### 4.3 语义与红线

- 约束 B 不动：热更前 `migrate verify` 门禁（`migrate_gate`）原样，migrate 只把账本
  追平触发面进程内化；追平后 verify 自然通过。
- 无 down、不回滚：CLI 前向语义原样；失败经 json + 退出码上报，安装器自行决定
  「删版本目录 + 恢复锁」。
- **残余风险（api-manual 登记且仅登记这两条）**：① `${path:root}` 可指 trusted_paths
  内任意模块树（授权粒度 = 部署方 config 把关）；② exec.run 返回的 stderr 可能含
  迁移错误详情（CLI 已脱敏凭据）。

### 4.4 测试

CLI 单测（json 形状、退出码、定向目录、脱敏）；e2e 经 exec.run 走通 sqlite 全链路；
mysql/pg 真库 DSN 环境惯例（`OJ_TEST_MYSQL`/`OJ_TEST_PG` env-gated + 静默 skip 对齐
既有插件测试惯例）；幂等重跑（账本去重）；白名单未含 migrate 模板 → NotCapable；
`--baseline` 注入尝试 → 模板失配拒绝。

---

## 5. PR-5：YAML 读写原语（parse + stringify）

```ts
yaml.parse(text: string): unknown          // 只读解析
yaml.stringify(value: object): string      // 仅供机器产物 manifests.yaml 写回
```

- `serde_yaml`（0.9.34+deprecated）已是直接依赖，零新增；同步实现放
  `spawn_blocking` + 独立 deadline（安全阻塞项 2 修复：同步 Rust op 不可被
  KillSwitch 打断，指数解析会占死池槽）。
- **解析三预算**（对抗性用例钉死）：① 输入 1MB；② **节点总数 + alias 引用数上限**
  （libyaml 只防直接递归锚，链式指数放大不设防——唯一真正封死 billion laughs 的
  手段）；③ 嵌套深度上限（约 20 万 `[` 即合法 1MB 输入，递归下降栈溢出是
  SIGSEGV 不可 catch——**必须**，非「必要时」）。实现形态：自写递归
  `Deserialize` 访问器（每层计数），直接产出 JS 形状并挂 `jsnum::sanitize_js_numbers`
  出口护栏（同 `crypto.rs:140` 先例）。
- 错误语义：非法 YAML / 超预算 → **抛错点名**（serde_yaml 带行列则附）；
  `stringify` 只承诺机器产物往返（config.yaml 禁用——丢注释，§1.6）。
- 重复键行为：先写对抗用例钉死 serde_yaml 实际行为再落文档承诺。
- deprecated 供应链信号记入阶段小结，评估替代（如 `serde-yaml-ng`）但不阻塞本批。

---

## 6. PR-6：远程 api mount（远期，只记方向）

`mounts[].api` 支持 URL 形态 = 装配期 admission gate 扩展（扩展点位置：
`oj/src/serve_cmd.rs` 装配层）：下载 → sha256/SRI + 平台签名校验 → 内容寻址缓存
（缓存目录归属：fs.root 内 vs 系统缓存，**立项决策项**）→ 按本地目录同型装配。
前置：PR-2/PR-5 之外还牵涉缓存生命周期与离线部署策略——单独立项设计后排期。

---

## 7. 横切：依赖 / 排序 / 版本节奏 / 测试 / 文档

- **依赖清单修正**：ring 直依（PR-1，spike 后）；**根 Cargo.toml tokio 显式加
  `"process"` feature**（现靠 deno/reqwest 上游 feature 统一在撑，`Cargo.toml:76`
  无 process——上游换 feature 集会静默崩）；serde_yaml 既有。除此之外零新依赖。
- **落地顺序**：① PR-1 → ② 地基（platform + exec + trusted_paths）→ ③ PR-5 →
  ④ PR-3 → ⑤ PR-4 CLI 增强 → ⑥ PR-6 远期。
- **配置落地清单**：新增 Config 字段同步 `known_top_level_keys` 对账表
  （`src/config.rs:1332-1346`，防漂移单测会红）。
- **文档落点（显式清单，防发版漏改）**：devkit 四件（api-manual 总表 27 组 +1 增至
  新全局组 `exec`/`server`/`yaml`、错误/限制表新条目、SKILL.md 陷阱速查、
  scenarios.md 安装闭环场景）、`sample/global.d.ts`（exec/yaml/server interface）、
  `docs/dev-guide.md`（新全局 + platform 段内部走读）、`docs/ops-manual.md`
  （tar 依赖与方言、`OJ_SECRET_KEY` 透传、tasks/WS 无特权语义）、`docs/bridge.md`
  （op↔global 对应面）。错误文案设计期定稿：`NotCapable: module 'x' is not
  platform-privileged` / 模板失配 / 输出超限 / YAML 预算超限等。
- **sample**：`sample/src/` 增最小 demo 模块（exec + yaml + reloadConfig 分步），
  `sample/config.yaml` 补 `platform:` 段示例（配置面唯一可运行参照物）。
- **版本节奏**：每步 bump 根 crate 版本 + CHANGELOG + 上述文档逐条对齐；无插件改动
  → ABI 11 不动、插件版本不 bump。`bootstrap.js` 挂新全局时保持 **7-bit ASCII** 红线。
- **测试惯例**：新能力全部配对抗性探测用例（§1.3.7/§1.4/§3/§4.4/§5 各清单）；
  异步测试 current_thread + deadline 等待，禁固定 sleep；symlink 用例
  try_symlink 探测特权失败即 skip。

---

## 8. 已否决备选（评审勿复活）

| 备选 | 否决理由 |
|------|----------|
| 内建 `fs.untarGz` | 用户明确不要内建；exec 更通用 |
| 类型化插件轴 `archive` | 需动 FFI 契约 + 重复实现 jail；成本高 |
| 泛型轴 `axis("archive")` | JSON 字符串通道传不了 bytes；绕开宿主 jail |
| 完整 shell（`sh -c` 字符串） | 注入面；白名单无从谈起 |
| 命名约定认定特权 | 第三方 bundle 可碰名抢注 |
| 签名即特权 | 分发信任 ≠ 运行时特权，两信任锚不混用 |
| 进程内 `migrate.run`（Extras 回调） | 用户改形：exec + CLI 增强，bridge 零改动 |
| 「白名单命令 + 自由 argv」exec 形态 | 安全评审阻塞项 1：tar 的
  `--use-compress-program`/`--to-command`/`-f` 等于 RCE —— 升级为 argv 模板 |
| `platform.patchConfig` 服务端原语（本轮） | trusted_paths + 文本手术三步模式已覆盖；
  受控 patch 原语列远期演进 |

---

## 9. 评审处置表（意见 → 处置）

### 安全（专员）

| # | 级别 | 意见 | 处置 |
|---|------|------|------|
| 1 | 阻塞 | argv 自由形态使 tar 白名单 = RCE | **采纳**：§1.1 升级 argv 模板（literal 定死 + 类型化占位符） |
| 2 | 阻塞 | yaml alias 放大 + 同步 op 不可抢占 DoS | **采纳**：§5 节点/alias 预算 + spawn_blocking |
| 3 | 阻塞 | reloadConfig × `_=>applied` 自提权链 | **采纳**：§3 platform/fs 归 restart_pending，列为验收项 |
| 4 | 重要 | cwd 非隔离 + 路径参数未治理 + 与 PR-4 矛盾 | **采纳**：§1.1 路径占位符双类（fs.root ∪ trusted_paths）；§1.3 措辞降级 |
| 5 | 重要 | platform.modules 按名可抢注 | **采纳**：§1.2 装配期「特权模块必须在平台受控目录」fail-fast |
| 6 | 重要 | PATH 解析与 env 空表矛盾；tar 方言 | **采纳**：§1.1 `oj`→current_exe 特判 + 宿主侧 PATH 解析；§1.4 方言入 smoke |
| 7 | 重要 | 预检 TOCTOU/成员名怪形态 | **采纳**：§1.4 安全判定后置（walk 校验权威化，预检降级快速失败） |
| 8 | 重要 | 归属请求级非调用栈级；tasks/WS 无归属 | **采纳**：§1.2 特权语义写死 + api-manual 限制表 |
| 9 | 建议 | migrate 前缀残余面（-c/--baseline/stderr） | **采纳**：§4.2 模板不含 --baseline/--db；§4.1 错误脱敏；§4.3 残余风险两条 |
| 10 | 建议 | decode_key 次序/base64url/双引擎 | **采纳**：§2.2 测试钉死 + 限制表 |
| 11 | 建议 | 进程组 kill 细节 | **采纳**：§1.3.5 process_group(0) + kill(-pgid)，cfg(unix) 门控，措辞降级 |

### 架构师

| # | 级别 | 意见 | 处置 |
|---|------|------|------|
| 1 | 阻塞 | Extras.reload 引 serve 类型 = 循环依赖 | **采纳**：§3 outcome 类型下沉根 crate，serve re-export |
| 2 | 重要 | Extras 冻结早于 ReloadState 存在（构造环） | **采纳**：§3 OnceLock 槽后填（照搬 set_executor 惯例），from_config 穿参 |
| 3 | 重要 | reload_lock 串行化 + 同步 executor 阻塞语义 | **采纳**：§3 共享锁 + spawn_blocking + actor 占用语义写明 |
| 4 | 重要 | tasks/WS 无归属恒 deny 未声明 | **采纳**：并入 §1.2 特权语义 |
| 5 | 重要 | 前缀序列 argv[0] 归一规则空白 | **被安全 1 模板化吸收**：模板首 token 宿主侧解析，无前缀匹配残余 |
| 6 | 建议 | known_top_level_keys 对账 | **采纳**：§7 落地清单 |
| 7 | 建议 | 文档清单不具体 | **采纳**：§7 显式文件清单 |

### 工程师

| # | 级别 | 意见 | 处置 |
|---|------|------|------|
| 1 | 阻塞 | PR-3 回调形态/注入点/构造环三缺陷 | **采纳**：与架构 1/2/3 同批并入 §3（同步形态 + 槽位 + spawn_blocking） |
| 2 | 重要 | tokio process feature 未显式声明 | **采纳**：§7 显式加 feature |
| 3 | 重要 | `"oj"` 解析无答案（PATH×空 env 矛盾） | **采纳**：§1.1 current_exe 特判 + 宿主侧解析，解析器可注入供测试 |
| 4 | 重要 | `tar -t` 无类型信息；判据子串误伤 | **采纳**：§1.4 后置 walk 权威化，预检仅快速失败；组件级判据 |
| 5 | 重要 | serde_yaml 深度必须 + 「包 Deserializer」不可行 | **采纳**：§5 自写递归 Deserialize 三预算 |
| 6 | 重要 | cwd 隔离表述夸大 | **采纳**：§1.3.2 措辞降级 + 路径治理移交占位符 |
| 7 | 建议 | kill_on_drop 不杀组；wait_with_output 无界；信号码 | **采纳**：§1.3.4/5/6 流式计数、kill(-pgid)、128+signal |
| 8 | 建议 | ring 引入链更正（jsonwebtoken 非 rustls） | **采纳**：§2.1 更正，结论不变 |
| 9 | 建议 | ArrayBuffer 归一 + bootstrap ASCII | **采纳**：§2.3 + §7 红线提醒 |
| 10 | 建议 | run_migrate fail-fast 重构；`<name>-<version>` 是新逻辑；stdout 已纯；timeout 偏紧 | **采纳**：§4.1 改动面清单；§1.3 timeout 文档写明显式调大 |
| 11 | 建议 | unchanged 返回形状未定义 | **采纳**：§3 unchanged=true + 空清单恒定形状 |

### 产品

| # | 级别 | 意见 | 处置 |
|---|------|------|------|
| 1 | 阻塞 | config.yaml 回写缺失且 jail 外够不到 | **采纳（择 a 变体）**：§1.6 `platform.trusted_paths` 扩 fs 轴 + 三步模式；`patchConfig` 列远期（b/c 两案不取：b 新原语面大，c 需新 CLI 子命令且丢注释问题同样存在） |
| 2 | 重要 | manifests.yaml 写回无序列化原语 | **采纳（改形）**：PR-5 扩为 parse + stringify（§5），stringify 仅限机器产物；「dist 禁手工编辑」语义在 api-manual 限定为「程序化经 stringify 写回」 |
| 3 | 重要 | 落位原子性未设计 | **采纳**：§1.5 原子落位次序五步 |
| 4 | 重要 | 退出码/解码语义未钉死 | **采纳**：§1.3.6 返回 vs 抛错边界 + UTF-8 强制 + 128+signal |
| 5 | 重要 | argv 路径参数 jail 语义未定义 | **采纳**：并入 §1.1 路径占位符（被安全 4 吸收） |
| 6 | 重要 | 文档落点无 PR 级清单 | **采纳**：§7 显式清单 + 错误文案设计期定稿 |
| 7 | 建议 | sample 覆盖缺位 | **采纳**：§7 demo 模块 + config 示例 |
| 8 | 建议 | yaml.parse 失败行为未定 | **采纳**：§5 抛错点名带行列 |
| 9 | 建议 | verifyEd25519 限制进表 | **采纳**：§2.4 限制表三条 |
| 10 | 建议 | ojm-installer 编排归属未声明 | **采纳**：§0 范围声明 |
| 11 | 建议 | PR-6 两锚点 | **采纳**：§6 admission gate 位置 + 缓存目录决策项 |

### 不采纳 / 保留意见

无全盘不采纳项。两处**择优而非照单**已注明理由：产品 1（三案取 a 变体）、
产品 2（stringify 进 PR-5 而非 CLI 化）。
