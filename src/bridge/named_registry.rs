//! 五轴注册表公共件：存储 + 重名 fail fast + 注册序自省（spec §2 泛型化裁决）。
//! 各轴在其上包一层实现自己的冲突/认领语义（db 查 scheme 交集、其余查名字）。

use std::collections::HashMap;
use std::sync::Arc;

use super::BridgeResult;

pub struct NamedRegistry<T: ?Sized> {
    items: HashMap<String, Arc<T>>,
    order: Vec<String>, // 注册顺序，自省展示用
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
}
