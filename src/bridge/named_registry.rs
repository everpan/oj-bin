//! 五轴注册表公共件：存储 + 重名 fail fast + 注册序自省（spec §2 泛型化裁决）。
//! 各轴在其上包一层实现自己的冲突/认领语义（db 查 scheme 交集、其余查名字）。

use std::collections::HashMap;
use std::sync::Arc;

use super::BridgeResult;

pub struct NamedRegistry<T: ?Sized> {
    items: HashMap<String, Arc<T>>,
    order: Vec<String>, // 注册顺序，自省展示用
    /// 默认别名：字面 "default" 解析到此名（CLI `--<key>` 选定某命名 profile 作默认源）。
    /// None → 字面 "default" 仍按同名查找（向后兼容既有 `backends.default` 写法）。
    default_alias: Option<String>,
}

impl<T: ?Sized> Default for NamedRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> NamedRegistry<T> {
    pub fn new() -> Self {
        Self {
            items: HashMap::new(),
            order: Vec::new(),
            default_alias: None,
        }
    }
    /// 重名 → Err（插件 vs 插件、插件 vs 内置均不允许覆盖，spec §2 注册冲突语义）。
    pub fn register(&mut self, name: &str, item: Arc<T>) -> BridgeResult<()> {
        if self.items.contains_key(name) {
            return Err(format!("registry: duplicate name '{name}'").into());
        }
        self.items.insert(name.to_string(), item);
        self.order.push(name.to_string());
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<Arc<T>> {
        self.items.get(name).cloned()
    }
    pub fn contains(&self, name: &str) -> bool {
        self.items.contains_key(name)
    }
    /// 设置默认别名：字面 "default" 解析到 `name`。`name` 不存在 → fail-fast（列出已注册名）。
    /// 与 `--<key> profile` 语义对齐——选错 profile 直接报错，不静默回落 default。
    pub fn set_default_alias(&mut self, name: &str) -> BridgeResult<()> {
        if !self.items.contains_key(name) {
            let mut names: Vec<&str> = self.names().collect();
            names.sort_unstable();
            return Err(format!("profile '{name}' not declared (available: {names:?})").into());
        }
        self.default_alias = Some(name.to_string());
        Ok(())
    }
    /// 字面 "default" 实际解析的 profile 名（别名或 "default"）。mq 等按名查找前先用它换算。
    pub fn default_name(&self) -> &str {
        self.default_alias.as_deref().unwrap_or("default")
    }
    /// 解析默认 profile 的实例：别名指向的 profile，或名为 "default" 的 profile。
    pub fn default(&self) -> Option<Arc<T>> {
        self.get(self.default_name())
    }
    /// 按注册顺序遍历名字（op_plugins 自省用）。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(String::as_str)
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_get_and_names_in_order() {
        let mut r: NamedRegistry<i32> = NamedRegistry::new();
        r.register("b", Arc::new(2)).unwrap();
        r.register("a", Arc::new(1)).unwrap();
        assert_eq!(*r.get("a").unwrap(), 1);
        assert_eq!(r.names().collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(r.len(), 2);
        assert!(r.get("missing").is_none());
    }

    #[test]
    fn duplicate_name_fails() {
        let mut r: NamedRegistry<i32> = NamedRegistry::new();
        r.register("x", Arc::new(1)).unwrap();
        let e = r.register("x", Arc::new(2)).unwrap_err();
        assert!(e.to_string().contains("duplicate name 'x'"));
        assert_eq!(*r.get("x").unwrap(), 1); // 未被覆盖
    }

    #[test]
    fn default_alias_redirects_literal_default() {
        let mut r: NamedRegistry<i32> = NamedRegistry::new();
        r.register("a", Arc::new(1)).unwrap();
        r.register("b", Arc::new(2)).unwrap();
        // 无别名时，字面 "default" 按同名查找（缺失）。
        assert!(r.default().is_none());
        assert_eq!(r.default_name(), "default");
        // 别名 "default" → b。
        r.set_default_alias("b").unwrap();
        assert_eq!(r.default_name(), "b");
        assert_eq!(*r.default().unwrap(), 2);
        // 别名指向不存在的 profile → fail-fast。
        let mut r2: NamedRegistry<i32> = NamedRegistry::new();
        r2.register("a", Arc::new(1)).unwrap();
        assert!(r2.set_default_alias("missing").is_err());
    }
}
