//! AuthGuard：HTTP 前置鉴权守卫契约（auth 解耦：实现迁入 oj-auth cdylib 插件，
//! core 只留 trait + FFI 适配）。server Pipeline 经 `Arc<dyn AuthGuard>` 消费。

/// 请求鉴权守卫。Ok(None) = 匿名路径放行；Ok(Some(user)) = 注入 http.user；Err = 401 消息。
/// ABI 9 四参：method（大写动词，WS 握手 = "GET"）与 headers（全部请求头 JSON，
/// 小写名 → 值；None = 无头）供 cookie 会话/CSRF 双提交消费；Bearer 形态可忽略后两参。
pub trait AuthGuard: Send + Sync {
    fn verify(
        &self,
        path_no_base: &str,
        method: &str,
        authorization: Option<&str>,
        headers: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String>;
}

#[cfg(test)]
mod tests {
    // 静态假 vtable：匿名 "/health"，token "good" → user，cookie/CSRF 形态，其余 Err。
    extern "C" fn fake_verify(
        path: oj_plugin_ffi::RString,
        method: oj_plugin_ffi::RString,
        auth: oj_plugin_ffi::RString,
        headers: oj_plugin_ffi::RString,
    ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RString, oj_plugin_ffi::RString> {
        let p: &str = &path;
        let m: &str = &method;
        let a: &str = &auth;
        let h: &str = &headers;
        if p == "/health" {
            return oj_plugin_ffi::RResult::Ok("null".into());
        }
        if a == "Bearer scalar" {
            return oj_plugin_ffi::RResult::Ok("42".into());
        }
        if a == "Bearer good" {
            return oj_plugin_ffi::RResult::Ok(r#"{"id":"1","roles":["admin"]}"#.into());
        }
        // cookie 会话形态假实现：GET + oj_sess cookie → user；POST 查 x-csrf-token。
        if m == "GET" && h.contains(r#""cookie":"oj_sess=good""#) {
            return oj_plugin_ffi::RResult::Ok(r#"{"id":"2","roles":[]}"#.into());
        }
        if m == "POST" && h.contains(r#""x-csrf-token":"t1""#) {
            return oj_plugin_ffi::RResult::Ok(r#"{"id":"2","roles":[]}"#.into());
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
        assert!(g.verify("/health", "GET", None, None).unwrap().is_none());
        let u = g
            .verify("/me", "GET", Some("Bearer good"), None)
            .unwrap()
            .unwrap();
        assert_eq!(u["id"], "1");
        // ABI 9：cookie 会话（GET + session cookie）与 CSRF（POST + 头）透传插件。
        let u = g
            .verify("/me", "GET", None, Some(r#"{"cookie":"oj_sess=good"}"#))
            .unwrap()
            .unwrap();
        assert_eq!(u["id"], "2");
        let u = g
            .verify("/me", "POST", None, Some(r#"{"x-csrf-token":"t1"}"#))
            .unwrap()
            .unwrap();
        assert_eq!(u["id"], "2");
        assert!(g.verify("/me", "GET", Some("Bearer bad"), None).is_err());
        assert!(g.verify("/me", "GET", None, None).is_err());
        // 契约外形状：非 object（标量）→ Err，不注入 http.user。
        assert!(g.verify("/me", "GET", Some("Bearer scalar"), None).is_err());
    }
}
