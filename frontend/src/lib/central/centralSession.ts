/**
 * 中枢对话的固定 Session ID。
 *
 * 中枢对话单独存储（设计稿 §13.1 `central_conversation`；后端见
 * `src-tauri/src/central.rs`）：文件位于 `{storage_root}/central/sessions/`，
 * 不与普通会话混存——`get_sessions`、删除、回收站等普通链路不触达。
 * 首次发送时创建，此后跨重启延续；值必须与后端 `CENTRAL_SESSION_ID` 一致。
 */
export const CENTRAL_SESSION_ID = 'central-hub';
