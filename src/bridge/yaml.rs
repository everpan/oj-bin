//! yaml.parse / yaml.stringify（ojm 批次 PR-5）：三预算防炸弹的 YAML 读写原语。
//!
//! 纯解析原语，无特权要求（所有模块可用，不占 platform 授权面）。
//!
//! **三预算**（对抗性输入钉死；超限一律抛错点名）：
//! ① 输入 1 MiB（`MAX_INPUT_BYTES`，`parse_text` 入口即拒）；
//! ② 节点总数 100_000（`MAX_NODES`）——自写递归 `DeserializeSeed` 访问器对**每个**
//!    节点（标量/序列/映射，含 alias 重放展开出的每个节点）计数。这是封死
//!    billion laughs 的主闸门：libyaml 只防直接递归锚，`a1:[*a0,*a0]` 链式放大
//!    在展开侧计节点，超限即断。alias 重放在 serde 数据模型里与普通节点不可区分
//!    （serde_yaml 以事件重放实现 alias），故不设独立 alias 计数器；serde_yaml
//!    自带的 alias 重放上限（jumpcount > events×100，错误文案 "repetition limit
//!    exceeded"）是更早触发的同向闸门，映射为 `yaml.parse: alias budget exceeded`
//!    点名（见 `map_parse_error`）。宽 alias 实测：5 万节点锚 × 100 引用（repetition
//!    guard 预算内）由本节点预算在 100_000 处截断。
//! ③ 嵌套深度 128（`MAX_DEPTH`）——约 20 万 `[` 是合法 1MB 输入，递归下降栈溢出
//!    是 SIGSEGV 不可 catch；深度检查在 seed 入口，先于 serde_yaml 的
//!    `visit_sequence`/`visit_mapping` 递归。
//!
//! **实现形态**：自写递归访问器直接产出 `serde_json::Value`——不走
//! `serde_yaml::Value` 中转（双倍内存且预算无处插）。serde_yaml 0.9.34 自带
//! 深度 128（`remaining_depth`）与重放上限双重护栏（设计稿误判"不设防"，实测
//! 更正），本模块预算与之叠加且更紧。
//!
//! **出口护栏**：产出过 `jsnum::sanitize_js_numbers`（同 `crypto.rs` jwt claims
//! 先例）——YAML 大整数经 v8 会变 BigInt，`json.ok` 直接 500。i64/u64 内的
//! 超安全位数在 sanitize 降级为十进制字符串；i128/u128 级在访问器内即转字符串。
//!
//! **spawn_blocking + 独立 deadline**：同步 Rust op 不可被 KillSwitch 打断，
//! parse/stringify 整体放 `tokio::task::spawn_blocking`，op 侧
//! `tokio::time::timeout` 再包一层（固定 5s：预算内 1MB 解析实测毫秒级，deadline
//! 只作 hang 兜底；超限抛错点名，不占死池槽）。
//!
//! **stringify 范围承诺**：仅机器产物（manifests.yaml 往返）；**禁用于
//! config.yaml 回写**——serde_yaml 往返丢注释/空白，config 手术走文本三步模式
//! （设计稿 §1.6）。

use std::cell::Cell;

use deno_core::op2;
use deno_error::JsErrorBox;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// 预算①：输入字节上限（1 MiB）。
pub const MAX_INPUT_BYTES: usize = 1 << 20;

/// 预算②：节点总数上限（含 alias 重放展开）。
///
/// 100_000 = serde_json::Value 单节点约 24-40B → 最坏 ~4MB 物化内存，比 1MB
/// 输入放大一个数量级以内；正常 manifests.yaml（千节点级）余量充分。
pub const MAX_NODES: usize = 100_000;

/// 预算③：嵌套深度上限（与 serde_yaml 内建 remaining_depth=128 同值，本模块
/// 先检查先触发，错误文案点名）。
pub const MAX_DEPTH: u32 = 128;

/// parse / stringify 的独立 deadline（spawn_blocking 不可取消，超时只解除
/// op 侧等待；预算已封死失控输入，此值仅作 hang 兜底）。
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

// ----- 有界解析访问器 -----

/// 每次解析一份预算账本（节点计数跨整棵树累计）。
struct Budget(Cell<usize>);

/// 种子：进入一个节点（计一次节点预算）并携带当前深度。
struct Seed<'a> {
    depth: u32,
    budget: &'a Budget,
}

impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        let n = self.budget.0.get() + 1;
        if n > MAX_NODES {
            return Err(de::Error::custom("yaml.parse: node budget exceeded"));
        }
        self.budget.0.set(n);
        if self.depth >= MAX_DEPTH {
            return Err(de::Error::custom(
                "yaml.parse: nesting depth limit exceeded (128)",
            ));
        }
        de.deserialize_any(Vis {
            depth: self.depth,
            budget: self.budget,
        })
    }
}

/// 访问器：直接产出 serde_json::Value（不中转 serde_yaml::Value）。
struct Vis<'a> {
    depth: u32,
    budget: &'a Budget,
}

impl<'de> Visitor<'de> for Vis<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a YAML value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    /// i128 级整数（serde_yaml 的 deserialize_any 对超出 i64/u64 的整数控量
    /// 走此路）：serde_json Value 不支持 i128，且超出 2^53-1 本就要降级——
    /// 直接转十进制字符串（与 jsnum 出口护栏同一形态）。
    fn visit_i128<E: de::Error>(self, v: i128) -> Result<Value, E> {
        if let Ok(i) = i64::try_from(v) {
            self.visit_i64(i)
        } else if let Ok(u) = u64::try_from(v) {
            self.visit_u64(u)
        } else {
            Ok(Value::String(v.to_string()))
        }
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Value, E> {
        if let Ok(u) = u64::try_from(v) {
            self.visit_u64(u)
        } else {
            Ok(Value::String(v.to_string()))
        }
    }

    /// NaN/Inf → 点名报错（取更严取向：静默 null 会吞掉上游数据错误）。
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        if !v.is_finite() {
            return Err(de::Error::custom(
                "yaml.parse: non-finite number (NaN/Inf) not supported",
            ));
        }
        match serde_json::Number::from_f64(v) {
            Some(n) => Ok(Value::Number(n)),
            None => Err(de::Error::custom(
                "yaml.parse: non-finite number (NaN/Inf) not supported",
            )),
        }
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(Seed {
            depth: self.depth + 1,
            budget: self.budget,
        })? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = map.next_key_seed(Seed {
            depth: self.depth + 1,
            budget: self.budget,
        })? {
            // serde_json 只接受字符串键：int/bool/null 键一律点名报错，
            // 不静默强转（强转会漂移语义，且 manifests.yaml 键恒为字符串）。
            let Value::String(k) = k else {
                return Err(de::Error::custom(
                    "yaml.parse: non-string mapping key not supported",
                ));
            };
            let v = map.next_value_seed(Seed {
                depth: self.depth + 1,
                budget: self.budget,
            })?;
            out.insert(k, v);
        }
        Ok(Value::Object(out))
    }
}

/// 错误映射：serde_yaml 的 Display 自带 path 前缀与 "at line N column M"
/// 后缀；本函数归一化为点名错误（预算类透传，语法类按规范格式重排）。
fn map_parse_error(e: serde_yaml::Error) -> String {
    let msg = e.to_string();
    // 自家访问器的点名错误：serde_yaml 已附 path/行列，原样透传。
    if msg.contains("node budget exceeded")
        || msg.contains("nesting depth limit exceeded")
        || msg.contains("non-string mapping key")
        || msg.contains("non-finite number")
    {
        return msg;
    }
    // serde_yaml 内建 alias 重放上限（jumpcount > events×100）——billion laughs
    // 链式放大的第一道闸门，映射为点名文案。
    if msg.contains("repetition limit exceeded") {
        return "yaml.parse: alias budget exceeded (alias replay amplification)".into();
    }
    // 多文档：serde_yaml 内建拒绝。
    if msg.contains("more than one document") {
        return "yaml.parse: expected a single YAML document (multi-document input rejected)"
            .into();
    }
    // 语法错：按规范格式重排为 `yaml.parse: invalid YAML at line N column M: <detail>`。
    if let Some(loc) = e.location() {
        let suffix = format!(" at line {} column {}", loc.line(), loc.column());
        let detail = msg.strip_suffix(&suffix).unwrap_or(&msg);
        return format!(
            "yaml.parse: invalid YAML at line {} column {}: {}",
            loc.line(),
            loc.column(),
            detail
        );
    }
    format!("yaml.parse: invalid YAML: {msg}")
}

/// 有界解析（纯函数，供 op 与单测共用；调用方负责放进 spawn_blocking）。
pub fn parse_text(text: &str) -> Result<Value, String> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "yaml.parse: input exceeds 1 MiB limit ({} bytes)",
            text.len()
        ));
    }
    // 空文档 = null（serde_yaml 对空输入的 deserialize_any 会报错，这里直接归约）。
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    let budget = Budget(Cell::new(0));
    // 单遍 deserialize_any；多文档由 serde_yaml 内建拒绝（映射为点名错误）。
    let value = Seed {
        depth: 0,
        budget: &budget,
    }
    .deserialize(serde_yaml::Deserializer::from_str(text))
    .map_err(map_parse_error)?;
    Ok(value)
}

/// 有界序列化（仅机器产物承诺；调用方负责放进 spawn_blocking）。
pub fn stringify_value(value: &Value) -> Result<String, String> {
    serde_yaml::to_string(value).map_err(|e| format!("yaml.stringify: {e}"))
}

// ----- ops -----

/// yaml.parse(text)：三预算 + spawn_blocking + 独立 deadline；出口过 jsnum 护栏。
#[op2]
#[serde]
pub async fn op_yaml_parse(#[string] text: String) -> Result<serde_json::Value, JsErrorBox> {
    let parsed = tokio::time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || parse_text(&text)),
    )
    .await
    .map_err(|_| JsErrorBox::generic("yaml.parse: timed out after 5s"))? // 预算已封死失控输入，此为 hang 兜底
    .map_err(|e| JsErrorBox::generic(format!("yaml.parse: blocking task failed: {e}")))?
    .map_err(JsErrorBox::generic)?;
    let mut v = parsed;
    super::jsnum::sanitize_js_numbers(&mut v);
    Ok(v)
}

/// yaml.stringify(value)：仅承诺机器产物（manifests.yaml）往返；config.yaml 禁用
/// （serde_yaml 往返丢注释，见模块注释）。
#[op2]
#[string]
pub async fn op_yaml_stringify(#[serde] value: serde_json::Value) -> Result<String, JsErrorBox> {
    tokio::time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || stringify_value(&value)),
    )
    .await
    .map_err(|_| JsErrorBox::generic("yaml.stringify: timed out after 5s"))?
    .map_err(|e| JsErrorBox::generic(format!("yaml.stringify: blocking task failed: {e}")))?
    .map_err(JsErrorBox::generic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Bridge;
    use crate::bridge::Extras;
    use crate::bridge::InMemoryKV;
    use crate::bridge::ModuleCtx;
    use crate::bridge::SchemaRegistry;
    use crate::bridge::module_loader::LoaderShared;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    // ----- 预算原语单测（不经 bridge） -----

    #[test]
    fn parse_plain_shapes() {
        let v = parse_text("a: 1\nb:\n  - x\n  - y: true\n").unwrap();
        assert_eq!(v["a"], serde_json::json!(1));
        assert_eq!(v["b"][0], serde_json::json!("x"));
        assert_eq!(v["b"][1]["y"], serde_json::json!(true));
        // 锚点引用普通用法：alias 展开为值拷贝。
        let v = parse_text("a: &x [1, 2]\nb: *x\n").unwrap();
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn parse_input_limit_named() {
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        let e = parse_text(&big).unwrap_err();
        assert!(e.contains("input exceeds 1 MiB limit"), "{e}");
        // 恰在预算内不触发。
        assert!(parse_text(&"x".repeat(MAX_INPUT_BYTES)).is_ok());
    }

    #[test]
    fn parse_node_budget_named() {
        // 宽 alias：一个 5 万节点锚 + 100 个引用（serde_yaml 重放上限预算内，
        // 节点展开 > 100_000）→ 节点预算点名，不 OOM。
        let mut text = String::from("a0: &x [");
        for i in 0..50_000 {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&i.to_string());
        }
        text.push_str("]\n");
        for i in 0..100 {
            text.push_str(&format!("k{i}: *x\n"));
        }
        let e = parse_text(&text).unwrap_err();
        assert!(e.contains("node budget exceeded"), "{e}");
    }

    #[test]
    fn parse_alias_chain_named() {
        // 链式放大（billion laughs 结构，每层自带锚名）：serde_yaml 重放上限
        // 先触发 → 映射为 alias budget exceeded 点名。
        let mut chain = String::from("a0: &a0 [0, 0]\n");
        for i in 1..=25 {
            chain.push_str(&format!("a{i}: &a{i} [*a{}, *a{}]\n", i - 1, i - 1));
        }
        let e = parse_text(&chain).unwrap_err();
        assert!(e.contains("alias budget exceeded"), "{e}");
    }

    #[test]
    fn parse_depth_limit_named_process_alive() {
        // 20 万 `[`（合法 1MB 输入）→ 深度超限点名；测试进程活着完成断言即证
        // 无 SIGSEGV（递归下降栈溢出不可 catch，唯有预算前置可防）。
        let deep = "[".repeat(200_000);
        let e = parse_text(&deep).unwrap_err();
        assert!(e.contains("nesting depth limit exceeded"), "{e}");
        // 预算内深度（127 层）正常解析。
        let ok = format!("{}0{}", "[".repeat(127), "]".repeat(127));
        assert!(parse_text(&ok).is_ok());
    }

    #[test]
    fn parse_invalid_yaml_with_location() {
        let e = parse_text("a: 1\n  b: [unclosed\n").unwrap_err();
        assert!(
            e.contains("yaml.parse: invalid YAML at line 2 column 4"),
            "{e}"
        );
        // 多文档 → 点名。
        let e = parse_text("a: 1\n---\nb: 2\n").unwrap_err();
        assert!(e.contains("expected a single YAML document"), "{e}");
    }

    #[test]
    fn parse_edge_values() {
        // 重复键（实测 serde_yaml 0.9.34：后键覆盖，不报错）——文档承诺跟随实测。
        assert_eq!(
            parse_text("a: 1\na: 2\n").unwrap(),
            serde_json::json!({"a": 2})
        );
        // NaN/Inf → 点名报错。
        assert!(parse_text("v: .nan\n").unwrap_err().contains("non-finite"));
        assert!(parse_text("v: .inf\n").unwrap_err().contains("non-finite"));
        // 非字符串键 → 点名报错。
        assert!(
            parse_text("1: a\n")
                .unwrap_err()
                .contains("non-string mapping key")
        );
        // i128 级大整数 → 十进制字符串。
        assert_eq!(
            parse_text("v: 123456789012345678901234567890\n").unwrap(),
            serde_json::json!({"v": "123456789012345678901234567890"})
        );
        // 空文档 → null。
        assert!(parse_text("").unwrap().is_null());
    }

    #[test]
    fn stringify_roundtrip_and_limits() {
        let v = serde_json::json!({
            "name": "user", "version": "0.1.0",
            "deps": {"a": "1.0"}, "nums": [1, 2.5, true],
        });
        let text = stringify_value(&v).unwrap();
        let back = parse_text(&text).unwrap();
        assert_eq!(back, v);
        // stringify 标量/数组也允许（机器产物最小承诺）。
        assert_eq!(stringify_value(&serde_json::json!("s")).unwrap(), "s\n");
    }

    #[test]
    fn stringify_preserves_key_insertion_order() {
        // 键序钉死（防行为漂移无告警）：deno_core 0.411 给 serde_json 统一开了
        // `preserve_order`（Cargo.lock：serde_json 依赖 indexmap）→ serde_json::Map
        // 是 IndexMap，stringify **保留插入序**（整数型键先行是 JS 侧对象语义，Rust
        // 侧即构造序），并非字典序。若未来 deno_core 撤掉该 feature，Map 退回
        // BTreeMap（字典序）→ 本测试即红，api-manual 的键序承诺随之失效需复核。
        let mut m = serde_json::Map::new();
        m.insert("zebra".into(), serde_json::json!(1));
        m.insert("mango".into(), serde_json::json!(2));
        m.insert("apple".into(), serde_json::json!(3));
        let text = stringify_value(&Value::Object(m)).unwrap();
        assert_eq!(text, "zebra: 1\nmango: 2\napple: 3\n", "{text}");
    }

    // ----- BDD S1–S8（经 bridge：js 侧拿到 plain object / 抛错点名 / tick 探针不饿死） -----

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-yamltest-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const API_SRC: &str = r#"
export default {
  s1_legal: async () => json.ok(await yaml.parse("a: 1\nb:\n  - x\n  - y: true\nc: &z [7, 8]\nd: *z")),
  s2_invalid: async () => json.ok(await yaml.parse("a: 1\n  b: [unclosed\n")),
  s3_chain: async () => {
    let s = "a0: &a0 [0, 0]\n";
    for (let i = 1; i <= 25; i++) s += `a${i}: &a${i} [*a${i - 1}, *a${i - 1}]\n`;
    return json.ok(await yaml.parse(s));
  },
  s4_deep: async () => json.ok(await yaml.parse("[".repeat(200000))),
  s5_over: async () => json.ok(await yaml.parse("x".repeat(1048577))),
  s6_bigint: async () => json.ok(await yaml.parse("v: 123456789012345678901234567890\nw: 9007199254740993\n")),
  s7_roundtrip: async () => {
    const m = { name: "user", version: "0.1.0", deps: { a: "1.0" }, nums: [1, 2.5, true] };
    const text = await yaml.stringify(m);
    const back = await yaml.parse(text);
    // 键序无关的语义相等（键序契约由 stringify_preserves_key_insertion_order 单测钉死）。
    const sortk = (v) => Array.isArray(v) ? v.map(sortk)
      : (v && typeof v === "object" ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sortk(v[k])])) : v);
    return json.ok({ equal: JSON.stringify(sortk(back)) === JSON.stringify(sortk(m)), text });
  },
  s8_tick_probe: async () => {
    // 事件循环 tick 探针（Issue 1，微任务形态）：先排一个 Promise.resolve().then()
    // 微任务探针，随后背靠背 push 20 个大 parse（不 await、中间无挂起点）。微任务
    // 回调只能在挂起点运行——spawn_blocking 下 op 调用只是 promise 调度（微秒级），
    // `await probe` 一到回调即跑（microMs ≈ 0）；若 parse 阻塞 isolate（spawn_blocking
    // 被移除的回归），20 个 push 的同步段把线程卡满，回调被拖到全部 parse 结束
    // （实测 ≈ bigMs）→ 阈值即红。探针不碰阻塞池，对池 CPU 争用免疫。
    // （不用 setTimeout(0)：bridge isolate 未注册 timers 全局。）
    let s = "items:\n";
    for (let i = 0; i < 20000; i++) s += "- key" + i + ": value" + i + "\n";
    let microMs = 0;
    const t0 = Date.now();
    const probe = Promise.resolve().then(() => { microMs = Date.now() - t0; });
    const ps = [];
    for (let i = 0; i < 20; i++) ps.push(yaml.parse(s));
    await probe;
    await Promise.all(ps);
    return json.ok({ microMs, bigMs: Date.now() - t0 });
  },
};
"#;

    fn make_bridge(base: &std::path::Path) -> Bridge {
        let dir = base.join("worker");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("api.js"), API_SRC).unwrap();
        let mut map = HashMap::new();
        map.insert(
            dir.to_string_lossy().into_owned(),
            ModuleCtx {
                name: "worker".into(),
                version: "0.1.0".into(),
                deps: Arc::new(HashSet::new()),
                db: None,
            },
        );
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: base.to_path_buf(),
                ts: false,
            })),
            Extras {
                modules: Arc::new(map),
                ..Default::default()
            },
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn yaml_bdd_main_scenarios() {
        let base = tmp("mods").canonicalize().unwrap();
        let b = make_bridge(&base);
        let api = base.join("worker").join("api.js");
        let t = std::time::Duration::from_secs(60);

        macro_rules! data {
            ($h:expr) => {{
                let cap = b
                    .run_module(&api, $h, Default::default(), t)
                    .await
                    .unwrap_or_else(|e| panic!("{} failed: {e}", $h));
                let v: Value = serde_json::from_slice(&cap.body)
                    .unwrap_or_else(|e| panic!("{} envelope: {e}", $h));
                assert_eq!(v["code"], 0, "{}: {}", $h, v["msg"]);
                v["data"].clone()
            }};
        }
        macro_rules! fails {
            ($h:expr, $part:expr) => {{
                let e = b
                    .run_module(&api, $h, Default::default(), t)
                    .await
                    .unwrap_err();
                let m = e.to_string();
                assert!(m.contains($part), "{} -> {m}", $h);
            }};
        }

        // S1：合法 YAML（嵌套 map/list/锚点）→ JS 侧 plain object。
        let d = data!("s1_legal");
        assert_eq!(d["a"], 1);
        assert_eq!(d["b"][0], "x");
        assert_eq!(d["b"][1]["y"], true);
        assert_eq!(d["d"][0], 7);

        // S2：非法 YAML → 抛错带行列。
        fails!("s2_invalid", "invalid YAML at line 2 column 4");

        // S3：alias 链式放大 → 预算超限点名，不 OOM。
        fails!("s3_chain", "alias budget exceeded");

        // S4：20 万 `[` → 深度超限报错，进程不崩。
        fails!("s4_deep", "nesting depth limit exceeded");

        // S5：>1MB → 输入超限。
        fails!("s5_over", "input exceeds 1 MiB limit");

        // S6：大整数 → sanitize 后 json.ok 不 500，值为字符串安全形态。
        let d = data!("s6_bigint");
        assert_eq!(d["v"], "123456789012345678901234567890");
        assert_eq!(d["w"], "9007199254740993");

        // S7：manifest 形对象 stringify → parse 往返语义相等。
        let d = data!("s7_roundtrip");
        assert_eq!(d["equal"], true, "roundtrip text: {}", d["text"]);

        // S8：事件循环 tick 探针（微任务形态，见 handler 注释）——20 个大 parse
        //（~450KB / ~6 万节点，预算内）背靠背 push 期间的 isolate 卡段直测。
        // spawn_blocking 下 microMs ≈ 0（op 调用微秒级）；阻塞实现下 microMs ≈
        // bigMs（20 × 单 parse 全程卡死，实验实测 264ms）。阈值 50ms 双侧余量
        // 充分且对阻塞池 CPU 争用免疫（探针不经池）。bigMs 带回作证据输出。
        let d = data!("s8_tick_probe");
        let micro_ms = d["microMs"].as_u64().unwrap_or(u64::MAX);
        let big_ms = d["bigMs"].as_u64().unwrap_or(0);
        assert!(
            micro_ms < 50,
            "event loop starved by big parse: micro={micro_ms}ms big={big_ms}ms"
        );
        println!("S8 evidence: microMs={micro_ms}ms bigMs={big_ms}ms");

        let _ = std::fs::remove_dir_all(&base);
    }
}
