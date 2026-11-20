//! 会话标题自动生成（自 core 迁入，lite 客户端机制随迁）。
//!
//! 投递用户消息且标题仍是默认值时，后台用 lite 端点（未配置时回退 chat）
//! 据用户消息生成标题，经 [`TiangongCore::set_title`] 的 only_if_default
//! 语义写回：turn 进行中经命令通道排队、空闲时直接落盘，两种路径都不会
//! 覆盖用户已手动修改的标题。

use tiangong_core::core::is_default_title;
use tiangong_llm::SingleProviderClient;

use super::CoreManager;

impl CoreManager {
    /// 投递用户消息后按需触发标题生成（fire-and-forget，失败静默）。
    ///
    /// 预检标题仍是默认值后才调模型，避免每次发消息都白花一次请求；
    /// 写回侧 `set_title(only_if_default=true)` 会再校验一次，预检与写回
    /// 之间用户手动改名不会丢失。生成时会话 Core 可能已回收，届时放弃
    /// 本次结果——下次发消息发现标题仍是默认值会再次触发，天然自愈。
    pub(crate) fn spawn_title_generation_if_needed(&self, session_id: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Ok(session) = self.load_session(session_id) else {
            return;
        };
        if !is_default_title(&session.title) {
            return;
        }
        // lite 端点不经 CoreConfig（模型端点已从中移除），从磁盘模型配置的
        // Lite 路由槽解析；未配置时回退 Chat 路由。标题生成低频，实时读盘
        // 可接受且能感知模型配置热更。无可用模型时放弃生成标题。
        let models = tiangong_config::io::load_models_config_at(&self.storage_root);
        let endpoint = models
            .resolve_slot(tiangong_llm::models_config::RoutingSlot::Lite)
            .or_else(|| models.resolve_slot(tiangong_llm::models_config::RoutingSlot::Chat))
            .map(tiangong_llm::ModelEndpoint::from_resolved);
        let Some(endpoint) = endpoint.filter(|endpoint| !endpoint.base_url.trim().is_empty())
        else {
            return;
        };
        let manager = self.clone();
        let sid = session_id.to_string();
        let input = text.to_string();
        tokio::spawn(async move {
            let client = SingleProviderClient::new(endpoint).with_session_id(sid.clone());
            let title = async {
                client.stream(tiangong_llm::TextRequest {
                    instruction: "你是会话标题生成助手。根据用户输入生成简洁的标题，要求：1. 标题不超过10个汉字2. 直接返回标题，不要任何解释或额外文字3. 标题要概括性强，简洁明了",
                    input: &input,
                }, &tiangong_llm::ModelRequest {
                    reasoning_effort: tiangong_llm::ReasoningEffort::None,
                    max_output_tokens: Some(200), temperature: Some(0.3), timeout_ms: Some(120_000),
                    ..Default::default()
                })?.collect_text().await
            }.await.ok();
            let Some(title) = title else {
                return;
            };
            let clean = title.trim().trim_matches('"').to_string();
            if clean.is_empty() {
                return;
            }
            let registry = manager.registry();
            if let Some(core) = registry.get(&sid) {
                let _ = core.set_title(clean, true);
            }
        });
    }
}
