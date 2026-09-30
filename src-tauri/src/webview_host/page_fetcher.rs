use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::webview_host::capability::PageFetcher;
use crate::webview_host::types::{
    BrowserCommand, BrowserPageSnapshot, BrowserResponse, ClickElementResult, ElementCandidate,
    FillFieldResult, FormExtractResult, LocateElementResult, QueryDomResult,
};

/// 通过 BrowserCommand channel 实现 PageFetcher trait
///
/// `session_id` 在 `on_session_ready` 时延迟注入（构造时 session 可能尚未就绪）。
pub struct BrowserPageFetcher {
    cmd_tx: mpsc::Sender<BrowserCommand>,
    session_id: RwLock<String>,
}

impl BrowserPageFetcher {
    pub fn new(cmd_tx: mpsc::Sender<BrowserCommand>) -> Self {
        Self {
            cmd_tx,
            session_id: RwLock::new(String::new()),
        }
    }

    /// 注入当前 session id（由 BrowserPlugin::on_session_ready 调用）。
    pub fn set_session_id(&self, session_id: &str) {
        if let Ok(mut guard) = self.session_id.write() {
            *guard = session_id.to_string();
        }
    }

    fn session_id(&self) -> Option<String> {
        let id = self.session_id.read().ok()?.clone();
        if id.trim().is_empty() {
            None
        } else {
            Some(id)
        }
    }

    pub fn cmd_tx(&self) -> mpsc::Sender<BrowserCommand> {
        self.cmd_tx.clone()
    }
}

macro_rules! send_and_wait {
    ($tx:expr, $cmd:expr, $rx:expr, $timeout:expr) => {{
        if $tx.send($cmd).await.is_err() {
            return None;
        }
        tokio::time::timeout(Duration::from_secs($timeout), $rx)
            .await
            .ok()?
            .ok()?
    }};
}

impl PageFetcher for BrowserPageFetcher {
    fn fetch_page(
        &self,
        url: &str,
        max_chars: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<BrowserResponse>> + Send>> {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        let url = url.to_string();
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let resp: BrowserResponse = send_and_wait!(
                tx,
                BrowserCommand::FetchPage {
                    session_id: session_id.clone(),
                    url,
                    max_chars,
                    show_panel: false,
                    response_tx
                },
                response_rx,
                35
            );
            Some(resp)
        })
    }

    fn observe_page(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<BrowserPageSnapshot>> + Send>>
    {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let snapshot: BrowserPageSnapshot = send_and_wait!(
                tx,
                BrowserCommand::ObservePage {
                    session_id: session_id.clone(),
                    response_tx,
                },
                response_rx,
                10
            );
            Some(snapshot)
        })
    }

    fn list_tabs(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<Vec<crate::webview_host::types::BrowserTab>>>
                + Send,
        >,
    > {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let tabs: Vec<crate::webview_host::types::BrowserTab> = send_and_wait!(
                tx,
                BrowserCommand::TabList {
                    session_id: session_id.clone(),
                    response_tx,
                },
                response_rx,
                10
            );
            Some(tabs)
        })
    }

    fn form_extract(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FormExtractResult>> + Send>>
    {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let result: FormExtractResult = send_and_wait!(
                tx,
                BrowserCommand::FormExtract {
                    session_id: session_id.clone(),
                    response_tx,
                },
                response_rx,
                10
            );
            Some(result)
        })
    }

    fn form_fill(
        &self,
        selector: &str,
        value: &str,
        strategy: &str,
        wait_for: Option<&str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FillFieldResult>> + Send>> {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        let selector = selector.to_string();
        let value = value.to_string();
        let strategy = strategy.to_string();
        let wait_for = wait_for.map(|s| s.to_string());
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let result: FillFieldResult = send_and_wait!(
                tx,
                BrowserCommand::FormFill {
                    session_id: session_id.clone(),
                    selector,
                    value,
                    strategy,
                    wait_for,
                    response_tx,
                },
                response_rx,
                15
            );
            Some(result)
        })
    }

    fn click_element(
        &self,
        selector: &str,
        wait_for: Option<&str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ClickElementResult>> + Send>>
    {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        let selector = selector.to_string();
        let wait_for = wait_for.map(|s| s.to_string());
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let result: ClickElementResult = send_and_wait!(
                tx,
                BrowserCommand::ClickElement {
                    session_id: session_id.clone(),
                    selector,
                    wait_for,
                    response_tx
                },
                response_rx,
                15
            );
            Some(result)
        })
    }

    fn load_html(
        &self,
        html: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<std::result::Result<(), String>>> + Send>,
    > {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        let html = html.to_string();
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            if tx
                .send(BrowserCommand::LoadHtml {
                    session_id: session_id.clone(),
                    html,
                    response_tx,
                })
                .await
                .is_err()
            {
                return None;
            }
            tokio::time::timeout(Duration::from_secs(10), response_rx)
                .await
                .ok()
                .and_then(|r| r.ok())
        })
    }

    fn locate_element(
        &self,
        query: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<LocateElementResult>> + Send>>
    {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        let query = query.to_string();
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let resp: LocateElementResult = send_and_wait!(
                tx,
                BrowserCommand::LocateElement {
                    session_id: session_id.clone(),
                    query,
                    response_tx,
                },
                response_rx,
                15
            );
            Some(resp)
        })
    }

    fn query_dom(
        &self,
        selector: &str,
        max_results: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<QueryDomResult>> + Send>> {
        let tx = self.cmd_tx.clone();
        let Some(session_id) = self.session_id() else {
            return Box::pin(async { None });
        };
        let selector = selector.to_string();
        Box::pin(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let result: QueryDomResult = send_and_wait!(
                tx,
                BrowserCommand::QueryDom {
                    session_id: session_id.clone(),
                    selector,
                    max_results,
                    response_tx
                },
                response_rx,
                10
            );
            Some(result)
        })
    }
}

/// 浏览器工具覆盖处理器（web_fetch / web_form_extract / web_form_fill / web_click / web_query_dom / web_locate_element）
pub struct BrowserToolOverride {
    fetcher: Arc<dyn PageFetcher>,
    /// web_fetch 执行锁——串行化对 WebView 的并发导航，规避 wry
    /// `url_from_webview` 在 WebView 导航中 `URL()` 返回 nil 导致的 panic
    /// （wry 0.54.3 / 0.55.1 均未修，参见上游 issue）。并发 web_fetch
    /// 调用按 FIFO 等待，而非并行操作同一 WebView。
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl BrowserToolOverride {
    pub fn new(fetcher: Arc<dyn PageFetcher>) -> Self {
        Self {
            fetcher,
            fetch_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn truncate_text(text: &str, max_chars: usize) -> String {
        let mut chars = text.chars();
        let mut value: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            value.push('…');
        }
        value
    }

    fn candidate_identity(candidate: &ElementCandidate) -> String {
        let mut parts = Vec::new();
        if !candidate.tag.is_empty() {
            parts.push(candidate.tag.clone());
        }
        if !candidate.role.is_empty() {
            parts.push(format!("role={}", candidate.role));
        }
        if !candidate.label.is_empty() {
            parts.push(format!(
                "label=\"{}\"",
                Self::truncate_text(&candidate.label, 60)
            ));
        }
        if !candidate.text.is_empty() && candidate.text != candidate.label {
            parts.push(format!(
                "text=\"{}\"",
                Self::truncate_text(&candidate.text, 60)
            ));
        }
        if parts.is_empty() {
            "未知元素".to_string()
        } else {
            parts.join(" ")
        }
    }

    fn format_target(target: Option<&ElementCandidate>, selector: &str) -> String {
        match target {
            Some(target) => {
                let actual_selector = if target.selector.is_empty() {
                    selector
                } else {
                    &target.selector
                };
                format!(
                    "目标：{}\n实际选择器：{}",
                    Self::candidate_identity(target),
                    actual_selector
                )
            }
            None => format!("实际选择器：{selector}"),
        }
    }

    fn format_candidates(candidates: &[ElementCandidate]) -> String {
        if candidates.is_empty() {
            return String::new();
        }

        let mut lines = vec!["候选元素：".to_string()];
        for (index, candidate) in candidates.iter().take(8).enumerate() {
            let selector = if candidate.selector.is_empty() {
                "(无选择器)"
            } else {
                &candidate.selector
            };
            let reason = if candidate.reason.is_empty() {
                "匹配"
            } else {
                &candidate.reason
            };
            let position = match (candidate.x, candidate.y) {
                (Some(x), Some(y)) => format!(" | 坐标：{x},{y}"),
                _ => String::new(),
            };
            lines.push(format!(
                "{}. {} | selector: {} | score: {} | reason: {}{}",
                index + 1,
                Self::candidate_identity(candidate),
                selector,
                candidate.score,
                reason,
                position
            ));
        }
        if candidates.len() > 8 {
            lines.push(format!("... 还有 {} 个候选", candidates.len() - 8));
        }
        lines.join("\n")
    }

    fn handle_web_fetch(
        fetcher: &Arc<dyn PageFetcher>,
        fetch_lock: &Arc<tokio::sync::Mutex<()>>,
        call: &tiangong_llm::tool::ToolCall,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        let mode = call
            .arguments
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("text");
        let url = match call.arguments.get("url").and_then(|v| v.as_str()) {
            Some(u) => u.to_string(),
            None => return Box::pin(async { None }),
        };
        // 本地路径（Unix / Windows 盘符 / UNC）→ file:// 的归一化统一在
        // 浏览器导航入口（normalize_navigation_url）完成，此处直接透传。

        if mode != "text" {
            return Box::pin(async { None });
        }
        let max_chars = call
            .arguments
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(12000) as usize;
        let fetcher = fetcher.clone();
        let fetch_lock = fetch_lock.clone();
        Box::pin(async move {
            // 串行化 web_fetch：并发调用等待前一个完成后再操作 WebView，
            // 规避 wry 在 WebView 导航中 URL() 返回 nil 的 panic。
            let _guard = fetch_lock.lock().await;
            let result = match fetcher.fetch_page(&url, max_chars).await {
                Some(r) => r,
                None => {
                    return Some(tiangong_core::tools::result::ToolResult {
                        ok: false,
                        summary: "浏览器获取页面失败".to_string(),
                        stdout: String::new(),
                        stderr: "浏览器无法获取页面内容".to_string(),
                        exit_code: 1,
                        execution: None,
                    });
                }
            };
            if !result.ok {
                return Some(tiangong_core::tools::result::ToolResult {
                    ok: false,
                    summary: format!(
                        "浏览器获取失败：{}",
                        result.error.clone().unwrap_or_default()
                    ),
                    stdout: String::new(),
                    stderr: result.error.unwrap_or_default(),
                    exit_code: 1,
                    execution: None,
                });
            }
            // 浏览器已导航到目标 URL。立即 observe_page 取渲染后的页面快照放入 ToolResult，
            // 保证本次工具调用直接返回正文（后台 watcher 仍会持续观察后续变化）。
            // observe 失败或超时时回退到 fetch_page 已拿到的 title/url。
            let (title, url_out, text) = match fetcher.observe_page().await {
                Some(snap) if !snap.url.is_empty() => {
                    let text = Self::truncate_text(&snap.text, max_chars);
                    (snap.title, snap.url, text)
                }
                _ => (
                    result.title.clone(),
                    result.final_url.clone(),
                    String::new(),
                ),
            };
            let stdout = if text.is_empty() {
                format!(
                    "已在浏览器中打开页面\n标题：{}\nURL：{}\n\n（页面内容将通过浏览器自动推送）",
                    title, url_out
                )
            } else {
                format!("标题：{}\nURL：{}\n\n{}", title, url_out, text)
            };
            Some(tiangong_core::tools::result::ToolResult {
                ok: true,
                summary: format!("浏览器已打开：{}", title),
                stdout,
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            })
        })
    }

    fn handle_web_form_extract(
        fetcher: &Arc<dyn PageFetcher>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        let fetcher = fetcher.clone();
        Box::pin(async move {
            let result = match fetcher.form_extract().await {
                Some(r) => r,
                None => {
                    return Some(tiangong_core::tools::result::ToolResult {
                        ok: false,
                        summary: "浏览器未打开，无法提取表单".to_string(),
                        stdout: String::new(),
                        stderr: "请先使用 web_fetch 打开页面".to_string(),
                        exit_code: 1,
                        execution: None,
                    });
                }
            };
            let total_fields: usize = result.forms.iter().map(|f| f.fields.len()).sum();
            let total_buttons: usize = result.forms.iter().map(|f| f.buttons.len()).sum();
            let json_output = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| "序列化表单数据失败".to_string());

            // 人类可读摘要
            let mut lines = Vec::new();
            for (fi, form) in result.forms.iter().enumerate() {
                if result.forms.len() > 1 {
                    lines.push(format!("表单 {}:", fi + 1));
                }
                for field in &form.fields {
                    let mut desc = format!(
                        "  字段 {}: {} type={} label=\"{}\"",
                        field.index + 1,
                        field.selector,
                        field.field_type,
                        field.label
                    );
                    if !field.placeholder.is_empty() {
                        desc.push_str(&format!(" placeholder=\"{}\"", field.placeholder));
                    }
                    if field.required {
                        desc.push_str(" [必填]");
                    }
                    if field.readonly {
                        desc.push_str(" [只读]");
                    }
                    if field.disabled {
                        desc.push_str(" [禁用]");
                    }
                    if !field.options.is_empty() {
                        let opts: Vec<String> = field
                            .options
                            .iter()
                            .map(|o| format!("{}={}", o.text, o.value))
                            .collect();
                        desc.push_str(&format!(" 选项：[{}]", opts.join(", ")));
                    }
                    lines.push(desc);
                }
                for (bi, btn) in form.buttons.iter().enumerate() {
                    let state = if btn.disabled { "disabled" } else { "enabled" };
                    lines.push(format!(
                        "  按钮 {}: <{}> [{}] ({}) {}",
                        bi + 1,
                        btn.tag,
                        state,
                        btn.selector,
                        btn.text
                    ));
                }
            }
            let human_summary = lines.join("\n");

            Some(tiangong_core::tools::result::ToolResult {
                ok: true,
                summary: format!(
                    "提取到 {} 个表单，共 {} 个字段，{} 个按钮",
                    result.forms.len(),
                    total_fields,
                    total_buttons
                ),
                stdout: format!("{human_summary}\n\n--- JSON ---\n{json_output}"),
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            })
        })
    }

    fn handle_web_form_fill(
        fetcher: &Arc<dyn PageFetcher>,
        call: &tiangong_llm::tool::ToolCall,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        let selector = call
            .arguments
            .get("selector")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let value = call
            .arguments
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let strategy = call
            .arguments
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("auto")
            .to_string();
        let wait_for = call
            .arguments
            .get("wait_for")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let fetcher = fetcher.clone();
        Box::pin(async move {
            let result = match fetcher
                .form_fill(&selector, &value, &strategy, wait_for.as_deref())
                .await
            {
                Some(r) => r,
                None => {
                    return Some(tiangong_core::tools::result::ToolResult {
                        ok: false,
                        summary: "浏览器未打开，无法填写字段".to_string(),
                        stdout: String::new(),
                        stderr: "请先使用 web_fetch 打开页面".to_string(),
                        exit_code: 1,
                        execution: None,
                    });
                }
            };
            let strategy_used = result.strategy.clone().unwrap_or_default();
            if result.ok {
                let wait_info = match &result.wait_result {
                    Some(w) if w.ok => {
                        format!("，等待条件满足：{}（{}ms）", w.condition, w.elapsed_ms)
                    }
                    Some(w) => format!("，等待超时：{}", w.error.as_deref().unwrap_or("超时")),
                    None => String::new(),
                };
                let diff_info = match &result.page_diff {
                    Some(d) if !d.is_empty() => format!("\n{d}"),
                    _ => String::new(),
                };
                Some(tiangong_core::tools::result::ToolResult {
                    ok: true,
                    summary: format!("字段填写成功（策略：{strategy_used}）{wait_info}{diff_info}"),
                    stdout: format!(
                        "已填写字段 {selector}，使用策略：{strategy_used}{wait_info}{diff_info}"
                    ),
                    stderr: String::new(),
                    exit_code: 0,
                    execution: None,
                })
            } else {
                Some(tiangong_core::tools::result::ToolResult {
                    ok: false,
                    summary: "字段填写失败".to_string(),
                    stdout: String::new(),
                    stderr: result.error.unwrap_or_default(),
                    exit_code: 1,
                    execution: None,
                })
            }
        })
    }

    fn handle_web_click(
        fetcher: &Arc<dyn PageFetcher>,
        call: &tiangong_llm::tool::ToolCall,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        let selector = call
            .arguments
            .get("selector")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let wait_for = call
            .arguments
            .get("wait_for")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let fetcher = fetcher.clone();
        Box::pin(async move {
            let result = match fetcher.click_element(&selector, wait_for.as_deref()).await {
                Some(r) => r,
                None => {
                    return Some(tiangong_core::tools::result::ToolResult {
                        ok: false,
                        summary: "浏览器未打开，无法点击元素".to_string(),
                        stdout: String::new(),
                        stderr: "请先使用 web_fetch 打开页面".to_string(),
                        exit_code: 1,
                        execution: None,
                    });
                }
            };
            if result.ok {
                let wait_info = match &result.wait_result {
                    Some(w) if w.ok => {
                        format!("，等待条件满足：{}（{}ms）", w.condition, w.elapsed_ms)
                    }
                    Some(w) => format!("，等待超时：{}", w.error.as_deref().unwrap_or("超时")),
                    None => String::new(),
                };
                let diff_info = match &result.page_diff {
                    Some(d) if !d.is_empty() => format!("\n{d}"),
                    _ => String::new(),
                };
                Some(tiangong_core::tools::result::ToolResult {
                    ok: true,
                    summary: format!("已点击元素 {selector}{wait_info}{diff_info}"),
                    stdout: diff_info,
                    stderr: String::new(),
                    exit_code: 0,
                    execution: None,
                })
            } else {
                let candidates_info = if result.candidates.is_empty() {
                    String::new()
                } else {
                    let cands: Vec<String> = result
                        .candidates
                        .iter()
                        .map(|c| format!("<{}> {} ({})", c.tag, c.text, c.selector))
                        .collect();
                    format!("。可能的目标：{}", cands.join("、"))
                };
                Some(tiangong_core::tools::result::ToolResult {
                    ok: false,
                    summary: "点击元素失败".to_string(),
                    stdout: String::new(),
                    stderr: format!("{}{candidates_info}", result.error.unwrap_or_default()),
                    exit_code: 1,
                    execution: None,
                })
            }
        })
    }

    fn handle_web_query_dom(
        fetcher: &Arc<dyn PageFetcher>,
        call: &tiangong_llm::tool::ToolCall,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        let selector = call
            .arguments
            .get("selector")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let max_results = call
            .arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(20) as usize;
        let fetcher = fetcher.clone();
        Box::pin(async move {
            let result = match fetcher.query_dom(&selector, max_results).await {
                Some(r) => r,
                None => {
                    return Some(tiangong_core::tools::result::ToolResult {
                        ok: false,
                        summary: "浏览器未打开，无法查询 DOM".to_string(),
                        stdout: String::new(),
                        stderr: "请先使用 web_fetch 打开页面".to_string(),
                        exit_code: 1,
                        execution: None,
                    });
                }
            };
            if result.elements.is_empty() {
                return Some(tiangong_core::tools::result::ToolResult {
                    ok: true,
                    summary: format!("选择器 \"{}\" 无匹配元素（共 0 个）", result.selector),
                    stdout: format!("选择器 \"{}\" 未匹配到任何元素", result.selector),
                    stderr: String::new(),
                    exit_code: 0,
                    execution: None,
                });
            }
            let mut lines = Vec::new();
            for el in &result.elements {
                let mut attrs_parts = Vec::new();
                for (k, v) in &el.attributes {
                    attrs_parts.push(format!("{k}=\"{v}\""));
                }
                let attrs_str = if attrs_parts.is_empty() {
                    String::new()
                } else {
                    format!(" {}", attrs_parts.join(" "))
                };
                lines.push(format!("元素 {}: <{}>{}", el.index, el.tag, attrs_str));
                if !el.text.is_empty() {
                    // char-safe 截断：len() 是字节数，按字节切片会切到 UTF-8 字符中间导致 panic。
                    let text_preview = Self::truncate_text(&el.text, 200);
                    for line in text_preview.split('\n') {
                        if !line.trim().is_empty() {
                            lines.push(format!("  {}", line.trim()));
                        }
                    }
                }
                lines.push(format!("  选择器: {}", el.selector));
            }
            let human_output = lines.join("\n");
            let json_output = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| "序列化结果失败".to_string());
            Some(tiangong_core::tools::result::ToolResult {
                ok: true,
                summary: format!(
                    "选择器 \"{}\"：共 {} 个匹配（返回 {} 个）",
                    result.selector, result.total, result.returned
                ),
                stdout: format!("{human_output}\n\n--- JSON ---\n{json_output}"),
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            })
        })
    }

    fn handle_web_locate_element(
        fetcher: &Arc<dyn PageFetcher>,
        call: &tiangong_llm::tool::ToolCall,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        let query = call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fetcher = fetcher.clone();
        Box::pin(async move {
            let result = match fetcher.locate_element(&query).await {
                Some(r) => r,
                None => {
                    return Some(tiangong_core::tools::result::ToolResult {
                        ok: false,
                        summary: "浏览器未打开，无法定位元素".to_string(),
                        stdout: String::new(),
                        stderr: "请先使用 web_fetch 打开页面".to_string(),
                        exit_code: 1,
                        execution: None,
                    });
                }
            };
            if result.ok {
                let target = result
                    .target
                    .as_ref()
                    .map(|t| Self::format_target(Some(t), &t.selector))
                    .unwrap_or_default();
                let candidates = Self::format_candidates(&result.candidates);
                let mut stdout = format!("定位成功。\n{target}");
                if !candidates.is_empty() {
                    stdout.push('\n');
                    stdout.push_str(&candidates);
                }
                Some(tiangong_core::tools::result::ToolResult {
                    ok: true,
                    summary: "元素定位成功".to_string(),
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                    execution: None,
                })
            } else {
                let candidates = Self::format_candidates(&result.candidates);
                let error = result.error.unwrap_or_default();
                let mut stdout = String::from("未找到匹配元素。");
                if !candidates.is_empty() {
                    stdout.push('\n');
                    stdout.push_str(&candidates);
                }
                Some(tiangong_core::tools::result::ToolResult {
                    ok: true,
                    summary: "未找到匹配元素".to_string(),
                    stdout,
                    stderr: error,
                    exit_code: 0,
                    execution: None,
                })
            }
        })
    }
}

impl tiangong_core::tools::extension::ToolOverrideHandler for BrowserToolOverride {
    fn handle(
        &self,
        call: &tiangong_llm::tool::ToolCall,
        _session: &mut tiangong_core::session::Session,
        _actor_id: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<tiangong_core::tools::result::ToolResult>>
                + Send,
        >,
    > {
        match call.name.as_str() {
            "web_fetch" => Self::handle_web_fetch(&self.fetcher, &self.fetch_lock, call),
            "web_form_extract" => Self::handle_web_form_extract(&self.fetcher),
            "web_form_fill" => Self::handle_web_form_fill(&self.fetcher, call),
            "web_click" => Self::handle_web_click(&self.fetcher, call),
            "web_query_dom" => Self::handle_web_query_dom(&self.fetcher, call),
            "web_locate_element" => Self::handle_web_locate_element(&self.fetcher, call),
            _ => Box::pin(async { None }),
        }
    }
}

/// 浏览器内容注入（ToolInput 实现）。
///
/// 页面加载完成或内容变化时，通过 AgentInput trait 统一投递到 Agent 对话链。
/// tool_name 为 `browser_data`，render 返回结构化 JSON。
pub struct BrowserContent {
    pub title: String,
    pub url: String,
    pub text: String,
    pub tabs: Vec<(String, String, String)>,
    pub active_tab_id: Option<String>,
    pub feedback: Option<String>,
}

impl tiangong_core::agent_input::ToolInput for BrowserContent {
    fn tool_name(&self) -> &str {
        "browser_data"
    }

    fn render(&self) -> serde_json::Value {
        serde_json::json!({
            "title": self.title,
            "url": self.url,
            "text": self.text,
            "tabs": self.tabs,
            "active_tab_id": self.active_tab_id,
            "feedback": self.feedback,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_text_ascii_short() {
        assert_eq!(BrowserToolOverride::truncate_text("hello", 200), "hello");
    }

    #[test]
    fn truncate_text_ascii_long() {
        let s = "a".repeat(300);
        let out = BrowserToolOverride::truncate_text(&s, 200);
        assert_eq!(out.chars().count(), 201); // 200 chars + 省略号
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_text_multibyte_no_panic() {
        // 中文：每个字符 3 字节，200 字符 = 600 字节。旧的字节切片 [&s[..200]]
        // 会切到 UTF-8 字符中间导致 panic；char-safe 截断不应 panic。
        let s = "中".repeat(300);
        let out = BrowserToolOverride::truncate_text(&s, 200);
        assert_eq!(out.chars().count(), 201);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_text_emoji_no_panic() {
        // emoji：每个字符 4 字节。验证不会因字节边界 panic。
        let s = "😀".repeat(300);
        let out = BrowserToolOverride::truncate_text(&s, 10);
        assert_eq!(out.chars().count(), 11); // 10 emoji + 省略号
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_text_exact_boundary() {
        // 刚好等于阈值：不应追加省略号
        let s = "a".repeat(200);
        let out = BrowserToolOverride::truncate_text(&s, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(!out.ends_with('…'));
    }
}
