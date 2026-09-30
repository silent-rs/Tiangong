use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// 页面快照/推送正文上限（字符数）。限制内原文完整推送；超出时保留头尾、
/// 中间以省略标记替代（见 [`clip_head_tail`]），Agent 需要被省略的内容时
/// 主动调用 `web_page_text` 按区间读取或按关键词搜索。
pub const PAGE_PUSH_MAX_CHARS: usize = 12_000;

/// 头尾截取：`text` 在 `limit` 字符内原样返回；超出时保留头部约 2/3、尾部
/// 约 1/3，中间替换为说明被省略区间与查询方式的标记，结果总长不超过
/// `limit`（`limit` 过小时退化为只保留头部）。按 Unicode 字符计数，不会
/// 切到多字节字符中间。
pub fn clip_head_tail(text: &str, limit: usize) -> String {
    clip_with_marker(text, limit, |start, end, total| {
        format!(
            "\n\n…[页面内容过长，已省略第 {start}–{end} 字（全文 {total} 字）；如需查看，请调用 web_page_text 按 offset 读取或按 keyword 搜索]…\n\n"
        )
    })
}

/// 页面变化文本的头尾截取：标记只说明省略字数（变化文本的位置与页面
/// 正文 offset 无关），并提示用 web_page_text 查看完整页面。
pub fn clip_change_text(text: &str, limit: usize) -> String {
    clip_with_marker(text, limit, |start, end, total| {
        // 预算估算时以 start==end 调用，此时用 total 估计省略字数的最大位数。
        let omitted = if end > start { end - start } else { total };
        format!(
            "\n\n…[变化内容过长，已省略中间 {omitted} 字；完整页面请调用 web_page_text 查询]…\n\n"
        )
    })
}

/// 头尾截取通用实现：`marker(start, end, total)` 生成省略标记。
fn clip_with_marker(
    text: &str,
    limit: usize,
    marker: impl Fn(usize, usize, usize) -> String,
) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    // 标记长度随数字位数变化，按最大可能位数（各位置均取 total）预估预算。
    let marker_len = marker(total, total, total).chars().count();
    let budget = limit.saturating_sub(marker_len);
    if budget == 0 {
        return text.chars().take(limit).collect();
    }
    let head_len = budget * 2 / 3;
    let tail_len = budget - head_len;
    let tail_start = total - tail_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(tail_start).collect();
    format!("{head}{}{tail}", marker(head_len, tail_start, total))
}

/// 浏览器标签
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTab {
    pub id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub source: BrowserTabSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_domain: Option<String>,
}

/// 标签创建来源。调用数据没有该字段时按用户标签处理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTabSource {
    #[default]
    User,
    Agent,
}

/// 标签列表响应（包含活跃标签 ID）
#[derive(Debug, Clone, Serialize)]
pub struct TabListResponse {
    pub tabs: Vec<BrowserTab>,
    pub active_tab_id: Option<String>,
}

/// 浏览器会话 Tab 快照
#[derive(Debug, Clone, Serialize)]
pub struct BrowserTabsSnapshot {
    pub session_id: Option<String>,
    pub tabs: Vec<BrowserTab>,
    pub active_tab_id: Option<String>,
}

/// Agent 请求在前端显示浏览器时携带的来源会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserOpenEvent {
    pub session_id: String,
    pub url: String,
}

/// Agent 正在使用浏览器（打开或导航页面）的信号。
///
/// 前端据此在浏览器图标上显示"使用中"标记，而不是自动弹出浏览器面板。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserAgentActiveEvent {
    pub session_id: String,
}

/// 页面加载事件。所有消费者必须按 `session_id` 路由，不能回退到当前活动会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserPageLoadedEvent {
    pub session_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub text: String,
}

/// 页面导航状态。前端按会话和标签过滤，避免并发导航串台。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNavigationStateKind {
    Loading,
    Loaded,
    Failed,
}

/// 页面导航状态事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserNavigationStateEvent {
    pub session_id: String,
    pub tab_id: String,
    pub navigation_id: u64,
    pub state: BrowserNavigationStateKind,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 浏览器事件队列及其来源会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserEventsEvent {
    pub session_id: String,
    pub tab_id: String,
    pub events: Vec<BrowserEvent>,
}

/// 浏览历史条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    pub timestamp: u64,
}

/// 标签页浏览历史结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabHistoryResult {
    pub tab_id: String,
    pub entries: Vec<HistoryEntry>,
    pub current_index: i32,
}

/// 浏览器命令（内部通道消息）
pub enum BrowserCommand {
    /// 获取网页内容（替代 web_fetch）
    FetchPage {
        session_id: String,
        url: String,
        max_chars: usize,
        /// 是否展开拓展区面板展示该页面（web_fetch open=true）；否则
        /// 仅静默建立标签（实例归属），不打扰用户。
        show_panel: bool,
        response_tx: oneshot::Sender<BrowserResponse>,
    },
    /// 打开 URL（用于链接点击等场景）
    OpenUrl { session_id: String, url: String },
    /// 获取当前浏览器页面的快照
    ObservePage {
        session_id: String,
        response_tx: oneshot::Sender<BrowserPageSnapshot>,
    },
    /// 提取页面表单结构
    FormExtract {
        session_id: String,
        response_tx: oneshot::Sender<FormExtractResult>,
    },
    /// 填写表单字段
    FormFill {
        session_id: String,
        selector: String,
        value: String,
        strategy: String,
        wait_for: Option<String>,
        response_tx: oneshot::Sender<FillFieldResult>,
    },
    /// 点击页面元素
    ClickElement {
        session_id: String,
        selector: String,
        wait_for: Option<String>,
        response_tx: oneshot::Sender<ClickElementResult>,
    },
    /// 加载本地 HTML 内容
    LoadHtml {
        session_id: String,
        html: String,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    /// 获取标签列表
    TabList {
        session_id: String,
        response_tx: oneshot::Sender<Vec<BrowserTab>>,
    },
    /// 新建标签
    TabNew { session_id: String, url: String },
    /// 切换标签
    TabSwitch { session_id: String, tab_id: String },
    /// 关闭标签
    TabClose { session_id: String, tab_id: String },
    /// 提取批注区域的元素信息
    AnnotationExtract {
        session_id: String,
        response_tx: oneshot::Sender<AnnotationExtractResult>,
    },
    /// 智能元素定位（不执行操作，仅查询候选）
    LocateElement {
        session_id: String,
        query: String,
        response_tx: oneshot::Sender<LocateElementResult>,
    },
    /// 用 CSS 选择器查询 DOM 元素
    QueryDom {
        session_id: String,
        selector: String,
        max_results: usize,
        response_tx: oneshot::Sender<QueryDomResult>,
    },
    /// Agent 主动查询当前页面正文：按区间读取或按关键词搜索（JSON 结果）。
    PageText {
        session_id: String,
        offset: usize,
        max_chars: usize,
        keyword: Option<String>,
        response_tx: oneshot::Sender<serde_json::Value>,
    },
    /// 获取标签页浏览历史
    TabHistory {
        session_id: String,
        tab_id: Option<String>,
        response_tx: oneshot::Sender<TabHistoryResult>,
    },
    /// 获取全局浏览历史（分页）
    GlobalHistory {
        session_id: String,
        offset: usize,
        limit: usize,
        response_tx: oneshot::Sender<Vec<HistoryEntry>>,
    },
}

/// 浏览器响应
#[derive(Debug, Clone)]
pub struct BrowserResponse {
    pub ok: bool,
    pub title: String,
    pub content: String,
    pub final_url: String,
    pub error: Option<String>,
}

/// 浏览器页面快照
#[derive(Debug, Clone)]
pub struct BrowserPageSnapshot {
    pub title: String,
    pub url: String,
    pub text: String,
    pub status: PageStatus,
    pub tabs: Vec<BrowserTab>,
    pub active_tab_id: Option<String>,
    pub events: Vec<BrowserEvent>,
}

/// 页面状态
#[derive(Debug, Clone)]
pub enum PageStatus {
    Loading,
    Loaded,
    Error(String),
}

/// 表单字段信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    pub index: usize,
    pub tag: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub name: String,
    pub id: String,
    pub label: String,
    pub placeholder: String,
    pub value: String,
    pub required: bool,
    pub readonly: bool,
    pub disabled: bool,
    pub selector: String,
    #[serde(default)]
    pub options: Vec<SelectOption>,
}

/// select 选项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub text: String,
}

/// 表单按钮
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormButton {
    pub tag: String,
    #[serde(rename = "type")]
    pub button_type: String,
    pub text: String,
    pub disabled: bool,
    pub selector: String,
}

/// 表单信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormInfo {
    pub fields: Vec<FormField>,
    #[serde(default)]
    pub buttons: Vec<FormButton>,
}

/// 表单提取结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormExtractResult {
    pub forms: Vec<FormInfo>,
}

/// 字段填写结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillFieldResult {
    pub ok: bool,
    pub strategy: Option<String>,
    pub error: Option<String>,
    #[serde(rename = "currentValue")]
    pub current_value: Option<String>,
    #[serde(default)]
    pub wait_result: Option<WaitResult>,
    #[serde(default)]
    pub page_diff: Option<String>,
}

/// 元素点击结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickElementResult {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub wait_result: Option<WaitResult>,
    #[serde(default)]
    pub candidates: Vec<ElementCandidate>,
    #[serde(default)]
    pub page_diff: Option<String>,
}

/// 等待条件结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitResult {
    pub ok: bool,
    pub condition: String,
    #[serde(rename = "elapsed")]
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

/// 候选元素信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementCandidate {
    pub selector: String,
    pub text: String,
    pub tag: String,
    pub role: String,
    pub label: String,
    pub score: i32,
    pub reason: String,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

/// 智能元素定位结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocateElementResult {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub ambiguous: bool,
    #[serde(default)]
    pub target: Option<ElementCandidate>,
    #[serde(default)]
    pub candidates: Vec<ElementCandidate>,
}

/// DOM 查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryDomResult {
    pub selector: String,
    pub total: usize,
    pub returned: usize,
    pub elements: Vec<QueryDomElement>,
}

/// DOM 查询到的单个元素
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryDomElement {
    pub index: usize,
    pub tag: String,
    pub text: String,
    pub attributes: HashMap<String, String>,
    pub selector: String,
    pub rect: DomRect,
}

/// DOM 元素的矩形位置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 浏览器语义事件（由 observer 模块产生）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BrowserEvent {
    #[serde(rename = "dialog_opened")]
    DialogOpened {
        timestamp: u64,
        #[serde(default)]
        detail: String,
    },
    #[serde(rename = "dialog_closed")]
    DialogClosed { timestamp: u64 },
    #[serde(rename = "content_changed")]
    ContentChanged {
        timestamp: u64,
        #[serde(default)]
        detail: String,
    },
    #[serde(rename = "user_click")]
    UserClick {
        timestamp: u64,
        element: String,
        text: String,
        selector: String,
    },
    #[serde(rename = "user_input")]
    UserInput {
        timestamp: u64,
        selector: String,
        label: String,
        value_length: usize,
    },
    #[serde(rename = "user_navigation")]
    UserNavigation { timestamp: u64, url: String },
    #[serde(rename = "network_response")]
    NetworkResponse {
        timestamp: u64,
        url: String,
        method: String,
        status: u16,
        #[serde(default)]
        detail: String,
    },
}

/// 页面变化反馈（推送给 Agent）。
///
/// 只反馈**页面本身的变化**（弹窗出现/关闭、内容更新、页面导航），并附带
/// 变化的具体内容；JS 交互（点击、输入）与接口响应等过程数据不单独反馈——
/// 它们若引起页面变化，会以变化内容的形式体现。整体按
/// [`PAGE_PUSH_MAX_CHARS`] 头尾截取，Agent 需要完整页面时主动 web_page_text。
pub fn format_browser_events(events: &[BrowserEvent]) -> Option<String> {
    let mut lines = Vec::new();
    for event in events {
        match event {
            BrowserEvent::DialogOpened { detail, .. } => {
                lines.push("[页面变化] 出现新的弹窗/覆盖层".to_string());
                push_detail(&mut lines, detail);
            }
            BrowserEvent::DialogClosed { .. } => {
                lines.push("[页面变化] 弹窗/覆盖层已关闭".to_string());
            }
            BrowserEvent::ContentChanged { detail, .. } => {
                lines.push("[页面变化] 页面内容已更新".to_string());
                push_detail(&mut lines, detail);
            }
            BrowserEvent::UserNavigation { url, .. } => {
                lines.push(format!("[页面变化] 页面导航到 {url}"));
            }
            // 过程数据：未引起页面变化时不反馈。
            BrowserEvent::UserClick { .. }
            | BrowserEvent::UserInput { .. }
            | BrowserEvent::NetworkResponse { .. } => {}
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(clip_change_text(&lines.join("\n"), PAGE_PUSH_MAX_CHARS))
    }
}

/// 事件是否属于页面变化（决定是否需要主动注入）。
pub fn is_page_change(event: &BrowserEvent) -> bool {
    matches!(
        event,
        BrowserEvent::DialogOpened { .. }
            | BrowserEvent::DialogClosed { .. }
            | BrowserEvent::ContentChanged { .. }
            | BrowserEvent::UserNavigation { .. }
    )
}

fn push_detail(lines: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        lines.push(text.to_string());
    }
}

/// 批注矩形区域信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 提取到的元素信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedElement {
    pub tag: String,
    pub text: String,
    pub attributes: HashMap<String, String>,
    pub selector: String,
    pub rect: AnnotationRect,
    pub overlap_ratio: f64,
    pub area: f64,
}

/// 单个批注区域的提取结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationRegionResult {
    pub annotation_index: usize,
    pub rect: AnnotationRect,
    pub elements: Vec<ExtractedElement>,
}

/// 批注元素提取结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationExtractResult {
    pub elements: Vec<AnnotationRegionResult>,
    pub count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_head_tail_keeps_text_within_limit() {
        assert_eq!(clip_head_tail("短文本", 100), "短文本");
        let exact = "a".repeat(100);
        assert_eq!(clip_head_tail(&exact, 100), exact);
    }

    #[test]
    fn clip_head_tail_keeps_head_and_tail_with_marker() {
        // 中文多字节字符：按字符计数，不得切断。
        let text: String = (0..5000)
            .map(|i| char::from_u32(0x4e00 + (i % 500)).unwrap())
            .collect();
        let clipped = clip_head_tail(&text, 1000);
        assert!(clipped.chars().count() <= 1000, "结果不得超过上限");
        let head: String = text.chars().take(50).collect();
        let tail: String = text.chars().skip(4950).collect();
        assert!(clipped.starts_with(&head), "保留头部");
        assert!(clipped.ends_with(&tail), "保留尾部");
        assert!(clipped.contains("全文 5000 字"));
        assert!(clipped.contains("web_page_text"));
    }

    #[test]
    fn clip_head_tail_marker_offsets_match_source() {
        let text: String = (0..3000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let clipped = clip_head_tail(&text, 600);
        let marker_start = clipped.find("\n\n…[").unwrap();
        let head_len = clipped[..marker_start].chars().count();
        let tail_len = clipped.chars().count()
            - clipped
                .find("]…\n\n")
                .map(|i| clipped[..i + "]…\n\n".len()].chars().count())
                .unwrap();
        assert!(clipped.contains(&format!("已省略第 {head_len}–{} 字", 3000 - tail_len)));
    }

    #[test]
    fn clip_head_tail_tiny_limit_degrades_to_head() {
        let text = "x".repeat(500);
        assert_eq!(clip_head_tail(&text, 10), "x".repeat(10));
    }

    #[test]
    fn annotation_rect_roundtrip() {
        let rect = AnnotationRect {
            x: 10.5,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        };
        let json = serde_json::to_string(&rect).unwrap();
        let back: AnnotationRect = serde_json::from_str(&json).unwrap();
        assert_eq!(back.x, 10.5);
        assert_eq!(back.y, 20.0);
        assert_eq!(back.width, 100.0);
        assert_eq!(back.height, 50.0);
    }

    #[test]
    fn extracted_element_roundtrip() {
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), "btn".to_string());
        attrs.insert("class".to_string(), "primary".to_string());

        let el = ExtractedElement {
            tag: "button".to_string(),
            text: "提交".to_string(),
            attributes: attrs,
            selector: "#btn".to_string(),
            rect: AnnotationRect {
                x: 50.0,
                y: 100.0,
                width: 80.0,
                height: 30.0,
            },
            overlap_ratio: 0.85,
            area: 2400.0,
        };
        let json = serde_json::to_string(&el).unwrap();
        let back: ExtractedElement = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag, "button");
        assert_eq!(back.text, "提交");
        assert_eq!(back.selector, "#btn");
        assert_eq!(back.overlap_ratio, 0.85);
        assert_eq!(back.attributes.get("id").unwrap(), "btn");
    }

    #[test]
    fn annotation_region_result_roundtrip() {
        let region = AnnotationRegionResult {
            annotation_index: 0,
            rect: AnnotationRect {
                x: 40.0,
                y: 40.0,
                width: 120.0,
                height: 50.0,
            },
            elements: vec![ExtractedElement {
                tag: "a".to_string(),
                text: "链接".to_string(),
                attributes: HashMap::new(),
                selector: "a[href=\"/test\"]".to_string(),
                rect: AnnotationRect {
                    x: 50.0,
                    y: 50.0,
                    width: 100.0,
                    height: 30.0,
                },
                overlap_ratio: 0.9,
                area: 3000.0,
            }],
        };
        let json = serde_json::to_string(&region).unwrap();
        let back: AnnotationRegionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.annotation_index, 0);
        assert_eq!(back.elements.len(), 1);
        assert_eq!(back.elements[0].tag, "a");
    }

    #[test]
    fn annotation_extract_result_roundtrip() {
        let result = AnnotationExtractResult {
            elements: vec![AnnotationRegionResult {
                annotation_index: 0,
                rect: AnnotationRect {
                    x: 0.0,
                    y: 0.0,
                    width: 200.0,
                    height: 100.0,
                },
                elements: vec![],
            }],
            count: 1,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: AnnotationExtractResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.count, 1);
        assert_eq!(back.elements.len(), 1);
    }

    #[test]
    fn extracted_element_from_bridge_json() {
        let json = r#"{
            "tag": "input",
            "text": "",
            "attributes": {"name": "email", "type": "email"},
            "selector": "[name=\"email\"]",
            "rect": {"x": 50, "y": 100, "width": 200, "height": 30},
            "overlap_ratio": 0.75,
            "area": 6000
        }"#;
        let el: ExtractedElement = serde_json::from_str(json).unwrap();
        assert_eq!(el.tag, "input");
        assert_eq!(el.attributes.get("name").unwrap(), "email");
        assert_eq!(el.overlap_ratio, 0.75);
    }

    #[test]
    fn network_response_deserialization() {
        let json = r#"{"type":"network_response","timestamp":1700000000,"url":"https://api.example.com/keys","method":"POST","status":200,"detail":"{\"key\":\"sk-abc123\"}"}"#;
        let event: BrowserEvent = serde_json::from_str(json).unwrap();
        match event {
            BrowserEvent::NetworkResponse {
                timestamp,
                url,
                method,
                status,
                detail,
            } => {
                assert_eq!(timestamp, 1700000000);
                assert_eq!(url, "https://api.example.com/keys");
                assert_eq!(method, "POST");
                assert_eq!(status, 200);
                assert!(detail.contains("sk-abc123"));
            }
            _ => panic!("Expected NetworkResponse variant"),
        }
    }

    #[test]
    fn network_response_array_deserialization() {
        let json = r#"[
            {"type":"network_response","timestamp":100,"url":"/a","method":"GET","status":200,"detail":"{}"},
            {"type":"content_changed","timestamp":200,"detail":"updated"},
            {"type":"network_response","timestamp":300,"url":"/b","method":"POST","status":201,"detail":"{\"id\":1}"}
        ]"#;
        let events: Vec<BrowserEvent> = serde_json::from_str(json).unwrap();
        assert_eq!(events.len(), 3);
        let network_count = events
            .iter()
            .filter(|e| matches!(e, BrowserEvent::NetworkResponse { .. }))
            .count();
        assert_eq!(network_count, 2);
    }

    #[test]
    fn network_response_default_detail() {
        let json =
            r#"{"type":"network_response","timestamp":1,"url":"/","method":"GET","status":204}"#;
        let event: BrowserEvent = serde_json::from_str(json).unwrap();
        match event {
            BrowserEvent::NetworkResponse { detail, .. } => {
                assert!(detail.is_empty());
            }
            _ => panic!("Expected NetworkResponse"),
        }
    }
}
