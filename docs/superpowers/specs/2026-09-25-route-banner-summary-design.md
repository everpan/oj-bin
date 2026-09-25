# 启动路由清单改统计输出 设计

日期：2026-09-25
需求：加载项目时不逐条输出路由，输出统计信息；出现错误与冲突时仍输出具体路由信息。

## 现状

`oj/src/app.rs` L890-907：启动 banner 把每个路由按「METHOD/PATH/FILE」等宽三列表逐行
打印（`table.grouped()` 展开）。大项目噪声大。
错误/冲突路径已逐条具体输出（用户要求已满足，零改动）：
- dev：`error: route: {method} {pattern} declared in {file1} and {file2}`（冲突）、
  `invalid route …`（非法）、`warn: route: …`（`_name_` 形态告警，v0.1.27）。
- release：`release routes: {failures joined}` 启动硬失败，含具体 pattern。

## 设计

1. 删除三列逐行循环，替换为一行统计：
   `routes: {method_rows} method-row(s), {patterns} pattern(s), {files} api file(s)`
   - method_rows = `table.listing().len()`（谓词×pattern 行）
   - patterns = listing 中 pattern 去重数（`HashSet`）
   - files = `table.grouped().len()`（api 文件数）
2. 不加明细开关（用户裁定）。
3. `warn_legacy_tail_wildcards` 走 `listing()` 内部数据，不受影响。
4. 文档：CHANGELOG v0.1.27 补一条；api-manual L106「路由表写进日志」措辞改统计。

## 测试

- 现有 oj app 测试若断言 banner 内容则适配（执行时查证；预期无）。
- 回归：`cargo test --release -p oj` 全绿 + fmt/clippy 门禁 + xtask 契约校验
  （api-manual 措辞变更后需 `cargo xtask build` 归置）。

## 非目标

不改动错误/冲突的输出形态；不新增配置项/环境变量；WS 挂载清单不在范围。
