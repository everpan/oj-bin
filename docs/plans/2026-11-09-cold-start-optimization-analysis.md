# 冷启动优化：分析、评审结论与落地状态

日期：2026-11-09
状态：P0/P1 已落地（代码在工作区）；**P2 冻结，待真实部署分相位数据**

## 1. 问题与现状分析

release 形态多版本部署：dist 下 `<module>-<v>/` 多版本共存，`manifests.yaml` 锁每模块
只激活一个版本——**磁盘旧版本目录不参与遍历，几乎零成本**（`oj/src/manifest.rs:151`）。

重启后的启动序列（`oj/src/app.rs` from_config ~1987 起）：

| 相位 | 行为 | 成本模型 |
|---|---|---|
| migrate 门禁 | release 默认 verify（M003/M004 校验，账本落后拒启） | O(模块数 × 每模块迁移数) 文件读+hash+账本 SELECT；无写 |
| seed 重放 | 无条件全量逐语句 exec（幂等靠 SQL 自身 + S002 预检） | O(模块数 × seed 语句数) 真实 DB 往返——**历史大头** |
| fixtures | 仅 oj test 触发，server 不灌 | — |
| ext_boot 预热 / 路由表 / bind | 装配与内省 | dev 模式路由表内省可能占大头 |

随模块数增长，冷启动耗时线性上升；每模块迁移数只增不减（refinery 只追加）。

## 2. 候选方案与评审结论

三方专家评审（运维 / 架构 / 开发，各自独立读代码）后裁决：

| 方案 | 结论 | 关键依据 |
|---|---|---|
| 1. seed `_oj_seeds(module, checksum)` 跳过账本 | **仅 opt-in，默认 off** | 架构契约异议：S006 允许 `ON CONFLICT DO UPDATE`/`OR REPLACE`（checks.rs:255,451-466），此类 seed 每次重放有可观察副作用（漂移修复），跳过即失效；DBA 误删行不再自愈。若做：~250-350 行，铁律=执行成功后落账、S002 保持全量预检、配 `seed_replay: always` 逃生口、CLI `--with-seed` 不接入账本 |
| 2. migrate verify 快路径 | **废弃** | refinery 0.9.2 `Migration::unapplied` 本就不解析 SQL、checksum 比对已是内存操作；「跳过文件解析」无可省之物，跳过读盘=放弃 M001 篡改检测（migrate.rs:716 测试钉死） |
| 3. 模块间并发 | **暂缓** | sqlite `max_connections(1)`（accessor_sqlx.rs:40）零收益；模块名字典序是事实上的跨模块依赖约定（seed.rs:938 测试钉死），并发打破它；mysql DDL 隐式提交无事务 |
| 4. 先 bind 后补 migrate/seed（业务 503） | **否决** | 逆转 ext_boot 前移的 fail-fast 决策（app.rs:2055-2061 注释）；与 daemon ready=bind 语义正面冲突，复刻"进程活着≠可用"信号歧义；M003/M004 拒启从 fail-fast 变 fail-silent |
| 5. systemd 化（ExecStartPre migrate + 探针） | **落地（文档）** | 零代码，fail-fast 语义完整保留；daemon 30s 硬超时整个消失 |

## 3. 已落地（P0 + P1）

### P0 分相位耗时日志（代码，未提交）
- `oj/src/app.rs`：迁移门禁锚点 `t_asm`，逐相位 eprintln 累计耗时：
  `oj: startup: migrate gate (auto|verify) …` / `seed replay …` / `fixtures …` /
  `ext_boot prewarm …` / `route tables …`
- `oj/src/serve_cmd.rs`：listening 行追加 `startup …` 总耗时（自 run_inner 起算）
- 行走 stderr，不受 RUST_LOG 影响；相邻两行相减 = 该相位净耗时
- sample（dev）实测：总 457ms，route tables 415ms 占大头，migrate 3.3ms / seed 1.9ms

### P1 systemd 托管（docs/ops-manual.md §2.0 + §6.0）
unit 模板：`Type=simple` + `ExecStartPre=oj migrate --with-seed` + `TimeoutStartSec`
按实测填 + `Restart=on-failure` + `ExecStartPost` curl 轮询 `{base}/health`。
注意：oj 无 sd_notify，禁止 `Type=notify`。

## 4. P2 决策协议（数据到位后执行）

拿到**真实部署（release 形态）**的 `oj: startup:` 相位日志后：

1. seed/migrate 占比确大 → 做方案 1 的 opt-in 实现（护栏见 §2）
2. route tables / ext_boot / 其他相位占大头 → 重排优化方向，勿执着于 seed 账本
3. 都不大 → 关闭此话题，仅保留 systemd 运维手段

## 5. 历史教训（本次事故关联）

上一事故的根因是调用方 `sleep + ss` 误判启动状态，而非启动慢。daemon 已自带就绪检查
（等 listening 才返回 0/1），调用方只须等命令返回看退出码。systemd 化后由
ExecStartPre + 探针接管该语义。
