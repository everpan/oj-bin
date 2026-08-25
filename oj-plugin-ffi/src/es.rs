//! es 轴 vtable（spec §3 保守形态，经 spike S.3 定案）。
//! 句柄语义同 tx 样例：connect 产 handle，close 释放；方法全返回 FfiFuture。

use crate::{FfiFuture, RString};

#[stabby::stabby]
#[repr(C)]
pub struct EsBackendVtable {
    pub search: extern "C" fn(handle: u64, index: RString, body: RString) -> FfiFuture,
    pub index_doc: extern "C" fn(handle: u64, index: RString, id: RString, body: RString) -> FfiFuture,
    pub delete_doc: extern "C" fn(handle: u64, index: RString, id: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
