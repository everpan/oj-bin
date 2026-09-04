//! AuthGuard：HTTP 前置鉴权守卫契约（auth 解耦：实现迁入 oj-auth cdylib 插件，
//! core 只留 trait + FFI 适配）。server Pipeline 经 `Arc<dyn AuthGuard>` 消费。

/// 请求鉴权守卫。Ok(None) = 匿名路径放行；Ok(Some(user)) = 注入 http.user；Err = 401 消息。
pub trait AuthGuard: Send + Sync {
    fn verify(
        &self,
        path_no_base: &str,
        authorization: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String>;
}

#[cfg(test)]
mod tests {
    // 静态假 vtable：匿名 "/health"，token "good" → user，其余 Err。
    extern "C" fn fake_verify(
        path: oj_plugin_ffi::RString,
        auth: oj_plugin_ffi::RString,
    ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RString, oj_plugin_ffi::RString> {
        let p: &str = &path;
        let a: &str = &auth;
        if p == "/health" {
            return oj_plugin_ffi::RResult::Ok("null".into());
        }
        if a == "Bearer good" {
            return oj_plugin_ffi::RResult::Ok(r#"{"id":"1","roles":["admin"]}"#.into());
        }
        oj_plugin_ffi::RResult::Err("missing or invalid bearer token".into())
    }

    static FAKE: oj_plugin_ffi::AuthGuardVtable = oj_plugin_ffi::AuthGuardVtable {
        verify: fake_verify,
    };

    #[test]
    fn ffi_auth_guard_maps_results() {
        let g = crate::bridge::ffi::FfiAuthGuard::new(&FAKE);
        use crate::bridge::AuthGuard;
        assert!(g.verify("/health", None).unwrap().is_none());
        let u = g.verify("/me", Some("Bearer good")).unwrap().unwrap();
        assert_eq!(u["id"], "1");
        assert!(g.verify("/me", Some("Bearer bad")).is_err());
        assert!(g.verify("/me", None).is_err());
    }
}
