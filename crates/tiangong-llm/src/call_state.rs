//! 响应内调用身份；上游 call_id 只作结果配对元数据，绝不作为聚合键。
use std::collections::HashMap;

use crate::error::LlmError;
use crate::stream::{CallKey, ProviderStreamEvent};

#[derive(Default)]
pub(crate) struct CallRegistry {
    items: HashMap<String, CallKey>,
    indexes: HashMap<u64, CallKey>,
    next: u64,
    key_indexes: HashMap<CallKey, u64>,
    key_items: HashMap<CallKey, String>,
    upstream_ids: HashMap<CallKey, String>,
}

impl CallRegistry {
    pub(crate) fn resolve(
        &mut self,
        item: Option<&str>,
        index: Option<u64>,
    ) -> Result<CallKey, LlmError> {
        let item = item.filter(|id| !id.trim().is_empty());
        let by_item = item.and_then(|id| self.items.get(id)).copied();
        let by_index = index.and_then(|index| self.indexes.get(&index)).copied();
        if let (Some(a), Some(b)) = (by_item, by_index)
            && a != b
        {
            return Err(LlmError::Serialization("工具输出项身份冲突".into()));
        }
        let key = match by_item.or(by_index) {
            Some(key) => key,
            None if item.is_some() || index.is_some() => {
                if let Some(index) = index {
                    CallKey(index)
                } else {
                    let key = CallKey(u64::MAX - self.next);
                    self.next += 1;
                    key
                }
            }
            None => return Err(LlmError::Serialization("工具事件缺少输出项身份".into())),
        };
        if let Some(index) = index {
            if self.key_indexes.get(&key).is_some_and(|old| *old != index) {
                return Err(LlmError::Serialization("同一输出项对应不同 index".into()));
            }
            self.key_indexes.insert(key, index);
        }
        if let Some(item) = item {
            if self.key_items.get(&key).is_some_and(|old| old != item) {
                return Err(LlmError::Serialization("同一 index 对应不同输出项".into()));
            }
            self.key_items.insert(key, item.to_owned());
            self.items.insert(item.to_owned(), key);
        }
        if let Some(index) = index {
            self.indexes.insert(index, key);
        }
        Ok(key)
    }

    pub(crate) fn item_events(
        &mut self,
        item: Option<&str>,
        index: Option<u64>,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        finished: bool,
    ) -> Result<Vec<ProviderStreamEvent>, LlmError> {
        let key = self.resolve(item, index)?;
        if !id.trim().is_empty() {
            if self
                .upstream_ids
                .get(&key)
                .is_some_and(|previous| previous != id)
            {
                return Err(LlmError::Serialization("同一输出项的 call_id 冲突".into()));
            }
            self.upstream_ids.insert(key, id.to_owned());
        }
        let mut events = vec![ProviderStreamEvent::ToolCallMetadata {
            key,
            id: self
                .upstream_ids
                .get(&key)
                .cloned()
                .unwrap_or_else(|| item.unwrap_or_default().to_owned()),
            name: name.to_owned(),
        }];
        if let Some(arguments) = arguments.filter(|args| !args.trim().is_empty()) {
            events.push(ProviderStreamEvent::ToolArgumentsSnapshot {
                key,
                arguments: arguments.to_owned(),
            });
        }
        if finished {
            events.push(ProviderStreamEvent::ToolCallFinished { key });
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_order_not_arrival_order_and_conflicting_aliases_fail() {
        let mut registry = CallRegistry::default();
        assert_eq!(registry.resolve(Some("b"), Some(1)).unwrap(), CallKey(1));
        assert_eq!(registry.resolve(Some("a"), Some(0)).unwrap(), CallKey(0));
        assert_eq!(registry.resolve(Some("a"), None).unwrap(), CallKey(0));
        assert!(registry.resolve(Some("a"), Some(1)).is_err());
        assert!(registry.resolve(Some("different"), Some(0)).is_err());
        assert!(registry.resolve(None, None).is_err());
    }
}

#[cfg(test)]
mod id_stability_tests {
    use super::*;
    #[test]
    fn missing_id_in_later_item_does_not_replace_real_call_id() {
        let mut registry = CallRegistry::default();
        let first = registry
            .item_events(Some("item-a"), Some(0), "real", "read", None, false)
            .unwrap();
        let last = registry
            .item_events(Some("item-a"), Some(0), "", "read", Some("{}"), true)
            .unwrap();
        assert!(matches!(&first[0],ProviderStreamEvent::ToolCallMetadata { id, .. } if id=="real"));
        assert!(matches!(&last[0],ProviderStreamEvent::ToolCallMetadata { id, .. } if id=="real"));
        assert!(
            registry
                .item_events(Some("item-a"), Some(0), "different", "read", None, false)
                .is_err()
        );
    }
}
