use std::collections::HashSet;

use anyhow::{anyhow, Result};
use tiangong_app_state::app_state::{InputCache, PendingTurnStub, TiangongState};

pub(crate) fn input_cache(state: &TiangongState, key: &str) -> InputCache {
    state.input_caches.get(key).cloned().unwrap_or_default()
}

pub(crate) fn set_input_cache(
    state: &mut TiangongState,
    key: &str,
    mut cache: InputCache,
) -> Result<(InputCache, bool)> {
    if key.trim().is_empty() {
        return Err(anyhow!("输入缓存键不能为空"));
    }
    if let Some(current) = state.input_caches.get(key) {
        if cache.revision < current.revision {
            return Ok((current.clone(), false));
        }
        cache.is_sending = current.is_sending;
    } else {
        cache.is_sending = false;
    }
    state.input_caches.insert(key.to_string(), cache.clone());
    Ok((cache, true))
}

#[doc(hidden)]
pub fn begin_input_send(state: &mut TiangongState, key: &str, revision: u64) -> Result<()> {
    let cache = state.input_caches.entry(key.to_string()).or_default();
    if cache.revision < revision {
        return Err(anyhow!(
            "输入缓存尚未更新到发送版本（期望 revision={revision}，当前 revision={}）",
            cache.revision
        ));
    }
    if cache.is_sending {
        return Err(anyhow!("当前输入已有消息正在准备发送"));
    }
    cache.is_sending = true;
    Ok(())
}

#[doc(hidden)]
pub fn finish_input_send(
    state: &mut TiangongState,
    key: &str,
    revision: u64,
    success: bool,
) -> Result<InputCache> {
    let cache = state.input_caches.entry(key.to_string()).or_default();
    cache.is_sending = false;
    if success && cache.revision == revision {
        cache.text.clear();
        cache.attachments.clear();
        cache.revision = cache.revision.saturating_add(1);
    }
    Ok(cache.clone())
}

pub(crate) fn has_pending_turn(state: &TiangongState, session_id: &str) -> bool {
    state.pending_turns.contains_key(session_id)
}

pub(crate) fn mark_pending_message(state: &mut TiangongState, session_id: &str, message_id: &str) {
    state
        .pending_turns
        .entry(session_id.to_string())
        .or_insert_with(|| pending_turn(session_id.to_string()))
        .queued_message_ids
        .insert(message_id.to_string());
}

pub(crate) fn accept_pending_message(
    state: &mut TiangongState,
    session_id: &str,
    message_id: &str,
) {
    let pending = state
        .pending_turns
        .entry(session_id.to_string())
        .or_insert_with(|| pending_turn(session_id.to_string()));
    pending.queued_message_ids.remove(message_id);
    pending.accepted_message_ids.insert(message_id.to_string());
}

pub(crate) fn complete_accepted_turn(state: &mut TiangongState, session_id: &str) {
    let should_remove = state
        .pending_turns
        .get_mut(session_id)
        .is_some_and(|pending| {
            pending.accepted_message_ids.clear();
            pending.legacy_pending = false;
            pending.queued_message_ids.is_empty()
        });
    if should_remove {
        state.pending_turns.remove(session_id);
    }
}

pub(crate) fn remove_pending_message(
    state: &mut TiangongState,
    session_id: &str,
    message_id: &str,
) {
    let should_remove = state
        .pending_turns
        .get_mut(session_id)
        .is_some_and(|pending| {
            pending.queued_message_ids.remove(message_id);
            pending.accepted_message_ids.remove(message_id);
            !pending.legacy_pending
                && pending.queued_message_ids.is_empty()
                && pending.accepted_message_ids.is_empty()
        });
    if should_remove {
        state.pending_turns.remove(session_id);
    }
}

pub(crate) fn clear_pending_turn(state: &mut TiangongState, session_id: &str) {
    state.pending_turns.remove(session_id);
}

fn pending_turn(session_id: String) -> PendingTurnStub {
    PendingTurnStub {
        session_id,
        queued_message_ids: HashSet::new(),
        accepted_message_ids: HashSet::new(),
        legacy_pending: false,
    }
}
