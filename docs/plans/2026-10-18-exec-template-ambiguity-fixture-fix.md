# 2026-10-18：exec 模板歧义（Windows CI 两条红的根因）与夹具修复评审

## 1. 现象

Windows CI 上 `src/bridge/exec.rs` 两个 `#[cfg(windows)]` BDD 用例红：

```
bridge::exec::tests::exec_bdd_main_scenarios_windows
  s4_env_on failed: NotCapable: exec template ambiguous: argv matches 2 authorized templates

bridge::exec::tests::exec_bdd_streaming_scenarios_windows
  assertion `left == right` failed  left: "compat\r\n"  right: "compat\n"
```

## 2. 根因

**红 1（S4 歧义）**——夹具 `worker_grant_win` 同时配了

- `cmd /c echo ${raw:word}`（S1/SS1/SS7/SS8 用）
- `cmd /c echo %${raw:NAME}%`（S4 env 白名单用，`%…%` 是 cmd 的变量定界符）

裸 raw 是 `%…%` 包裹 raw 的**超集**：任何 `%X%` 形态 argv 都同时命中两条，而
`template::match_all` 的契约是「≥2 命中 → `Ambiguous` 拒绝，不静默择一」（设计稿
`2026-10-16-ojm-native-exec-platform-apis-design.md:78`、api-manual §6）。v0.1.79 之前
`%${raw:NAME}%` 在**解析期**就失败（旧规则要求占位符占满整 token），整条 allow 解析失败、
根本走不到匹配期；commit `91d979a` 引入 `Token::Wrapped`（占位符可带字面前后缀）后它才
开始参与匹配，S4 于是首次撞上歧义。**生产行为正确，错的是夹具配置**——真实用户这么配
也会恒红，且报错不指路。

**红 2（SS8 行尾）**——上一版 `31c997f` 把 8 处行尾字面收敛到「行尾真相源」
（`echo_out` / `IN_TXT`）时漏了 SS8 这一处，仍写死 `"compat\n"`，而 cmd 的 `echo` 恒 CRLF。
典型的「修完一处、下一处接着红」。

## 3. 改动

1. **夹具消歧**（`exec.rs`）：`cmd /c echo` 家族由一条参数化模板改为每种取值一条
   **全字面**模板（`cmd /c echo hello` / `cmd /c echo x` / `cmd /c echo compat`），
   `%VAR%` 仍走 `cmd /c echo %${raw:NAME}%`。裸 raw 的覆盖不减弱——`oj /c echo ${raw:msg}`
   （resolver 同指 cmd.exe）、`cmd /c exit ${raw:code}`、`ping -n ${raw:count}` 覆盖正向，
   `sh -c ${raw:script}`（S3）覆盖 option-like / 绝对形态负向。
2. **SS8** 断言改 `echo_out("compat")`。
3. **新增平台无关单测** `windows_fixture_templates_are_unambiguous`：模板表抽为
   `WIN_TEMPLATES`（unix 上也编译），用例直接对**真夹具**断言「每个场景 argv 恰命中
   1 条」，并钉死反例——同位置裸 raw 与 `%…%` 包裹 raw → `Ambiguous{count:2}`；
   非 `%…%` argv 只命中裸 raw；`%…%` 内填 `-x` / `/abs` / `C:\x` 仍被拒（Windows 面删掉
   裸 echo 模板后，包裹 raw 的负向覆盖只在此处）。
4. **文档**：api-manual §6（占位符前后缀条目标注 v0.1.79 + 「同位置别既配裸 raw 又配
   包裹 raw」+ Windows 小节 `%VAR%` 写法条）、SKILL 陷阱速查一条、devkit README v0.1.79
   版本行、CHANGELOG（补 v0.1.79 缺失的「占位符字面前后缀」能力条目 + 本次 fix 条目）。
   生产代码零改动。

## 4. 四路评审口记与处置

| 路 | 结论 | 处置 |
|---|---|---|
| 工程师 | 批准；A 不丢语义（逐条核对 handler 表与断言）。要求：模板表抽常量、单测对**真夹具**断言（抽象形态钉不住夹具）；全仓再扫残留行尾字面 | 全部采纳（`WIN_TEMPLATES` + 真夹具断言；扫过，仅 SS8 一处残留） |
| 架构师 | 批准，不改生产语义：若改「更具体者优先」，命中哪条模板由 argv 形态决定 → `${path:}` jail 与绑定口径随输入漂移，授权不再是配置期常量；且已发布契约即「歧义拒绝」。建议 S4 改用 `cmd /c set ${raw:NAME}` 以保留参数化 echo 模板 | 前项采纳（维持 fail-closed）。**后项驳回**：`cmd /c set` 遇未定义变量退 1、报错走 stderr，中文 Windows 上是 GBK → 落 `exec: stderr is not valid UTF-8`，断言形态比现在更脆 |
| 产品 | 有条件批准：文档瘦身（§6 两处不重复写同一句）；CHANGELOG 缺「占位符带字面前后缀」这一 v0.1.79 用户可见能力条目，须先补齐 | 全部采纳（能力条目已补；§6 两处措辞分工：一处讲规则、一处给 Windows 例子）；`exec template ambiguous` **登记**列出命中模板原文（见 §5） |
| 安全 | 批准（未削弱边界；`check_raw` 确作用于中间子串，重建不产生新 token）。要求新单测补包裹 raw 的负向；指出 `check_raw` 不过滤 `%` / cmd 元字符 | 负向断言已补。**元字符过滤登记为 OJ-9**（见 §5） |

## 5. 刻意不做（登记）

| 项 | 说明 | 激活条件 |
|---|---|---|
| OJ-9 `check_raw` 未过滤 cmd 元字符 | 值为 `a%&calc` 时可提前闭合 `%…%` 定界、`&` 再分出第二条命令（**静态推演，本机无 Windows 可实证**）；裸 raw 在 `cmd /c` 下同样未过滤 `&`/`|`/`^`。属生产行为收紧（fail-closed）且 api-manual 已写明 raw 只拒 `-` 起手与绝对形态，需独立 spec + 版本分界 + Windows 实测 | 立 OJ-9 专项；带 Windows CI 实证再落地 |
| `exec template ambiguous` 报错只给命中条数 | 用户排障时不指路（本次正是因此绕了一圈）。改错误串是契约变更，需同步 api-manual / scenarios 两张错误表 + 回归用例 + 版本分界；模板是配置白名单、非机密，列出原文安全 | 下次 exec 相关版本分界提交时带上 |
| 交叉编译 `x86_64-pc-windows-msvc` 验证 | 本机装了该 target，但 `aws-lc-sys` 需要 Windows SDK 头文件（`windows.h`），`cargo check --target` 必败 | 改用 §6 的 cfg 翻转招（无需 Windows SDK） |

## 6. 验证手段（本机 macOS）

1. `cargo fmt --check` 绿（fmt 解析全部 cfg 分支）；
2. `cargo test --release windows_fixture_templates` 绿（新增单测，本机真跑）；
3. cfg 翻转：`#[cfg(windows)]` → `#[cfg(all())]` 后 `cargo check --release --tests -p only-js`
   ——Windows 夹具在本机真正类型检查一遍（仅剩既有的 `probe` 未用告警，与 `#[cfg(unix)]`
   成对，属预期），验完从备份还原；
4. `cargo test --release exec_` 全绿；
5. devkit：`cargo xtask build` 归置 `bin/devkit/` + `cargo test --release -p xtask`（契约用例）。
