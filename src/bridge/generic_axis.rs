//! 泛型轴通道：插件自报的非类型化轴（不在 plugin_loader::TYPED_AXES 内）经
//! op_axis_call + bootstrap axis() Proxy 暴露给 JS。
//! 信任边界与既有 vtable 相同：插件须按 GenericVtable 形状构造 vtable、
//! 经 catch_future 收敛 panic（panic=unwind 红线，宿主侧不 catch_unwind）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use oj_plugin_ffi::RString;

/// 泛型轴句柄：来源插件名（报错归因）+ vtable。
pub struct GenericAxisHandle {
    pub plugin: String,
    pub vt: &'static oj_plugin_ffi::GenericVtable,
}

/// 轴名 → 句柄（装配期冻结、只读；空表 = 无泛型轴，op 报 unknown axis）。
pub type GenericAxisRegistry = Arc<HashMap<String, GenericAxisHandle>>;

/// 装配入口：Registries.generic 的 (轴名, 插件名, vtable) 三元组 → 冻结注册表。
pub fn registry_from(
    entries: Vec<(String, String, &'static oj_plugin_ffi::GenericVtable)>,
) -> GenericAxisRegistry {
    Arc::new(
        entries
            .into_iter()
            .map(|(axis, plugin, vt)| (axis, GenericAxisHandle { plugin, vt }))
            .collect(),
    )
}

/// 单次泛型轴调用：JSON args 进 → JSON Value 出；future Err 原样透传为 JS 异常。
pub(crate) async fn call_axis(
    vt: &'static oj_plugin_ffi::GenericVtable,
    op: &str,
    args: &str,
) -> Result<serde_json::Value, JsErrorBox> {
    let fut = (vt.call)(RString::from(op), RString::from(args));
    let bytes = super::ffi::await_ffi(fut)
        .await
        .map_err(JsErrorBox::generic)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| JsErrorBox::generic(format!("axis op '{op}': plugin returned non-JSON ({e})")))
}

#[op2]
#[serde]
pub async fn op_axis_call(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] op: String,
    #[string] args: String,
) -> Result<serde_json::Value, JsErrorBox> {
    // 先查表取句柄，释放 OpState 借用再 await（同 op_mq_call 纪律）。
    let reg = {
        let s = state.borrow();
        s.borrow::<Arc<super::StableState>>().generic_axes.clone()
    };
    let handle = reg.get(&name).ok_or_else(|| {
        let avail: Vec<&str> = reg.keys().map(String::as_str).collect();
        JsErrorBox::generic(format!(
            "unknown generic axis '{name}' (available: {avail:?})"
        ))
    })?;
    call_axis(handle.vt, &op, &args).await
}

#[cfg(test)]
mod tests {
    use oj_plugin_ffi::{FfiFuture, RString, ready_err, ready_ok};

    fn fake_vt() -> &'static oj_plugin_ffi::GenericVtable {
        extern "C" fn call(op: RString, _args: RString) -> FfiFuture {
            if &op[..] == "ok" {
                ready_ok(br#"{"r":1}"#.to_vec())
            } else {
                ready_err("boom")
            }
        }
        Box::leak(Box::new(oj_plugin_ffi::GenericVtable { call }))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_dispatches_and_parses_json() {
        let vt = fake_vt();
        let out = super::call_axis(vt, "ok", "[]").await.unwrap();
        assert_eq!(out["r"], 1);
        let err = super::call_axis(vt, "bad", "[]").await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }
}
