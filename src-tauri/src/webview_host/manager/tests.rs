use super::*;

#[test]
fn rect_is_displayable_rejects_invalid_rects() {
    // 初始/收起后的失效矩形不可用于摆位（无头页面保持屏幕外）
    assert!(!rect_is_displayable((0.0, 0.0, 0.0, 0.0)));
    assert!(!rect_is_displayable((60.0, 60.0, 0.0, 720.0)));
    assert!(!rect_is_displayable((60.0, 60.0, 1024.0, 0.0)));
    assert!(!rect_is_displayable((60.0, 60.0, -1024.0, 720.0)));
    // 前端 instanceShow 下发的展示矩形宽高有效
    assert!(rect_is_displayable((60.0, 60.0, 1024.0, 720.0)));
    assert!(rect_is_displayable((0.0, 0.0, 1.0, 1.0)));
}

#[test]
fn session_directory_name_encoding_is_windows_legal() {
    // scope 含冒号（webview:browser:<id>），Windows 不允许冒号出现在
    // 文件名中；编码后只含字母数字与 %，且同 scope 编码稳定。
    let scope = "webview:browser:03guj8e2h1rko94862xx59zf2";
    let encoded = encode_session_directory_name(scope);
    assert_eq!(
        encoded,
        "session-webview%3Abrowser%3A03guj8e2h1rko94862xx59zf2"
    );
    assert_eq!(encoded, encode_session_directory_name(scope));
    assert!(!encoded.contains(':'));
    assert_ne!(
        encode_session_directory_name("webview:browser:a"),
        encode_session_directory_name("webview:browser:b")
    );
    // 非 ASCII（如中文会话名）按 UTF-8 字节稳定编码
    assert_eq!(
        encode_session_directory_name("会话"),
        "session-%E4%BC%9A%E8%AF%9D"
    );
}

#[test]
fn non_html_resource_url_detection_by_extension() {
    assert!(is_non_html_resource_url("file:///C:/Users/EDY/pic.PNG"));
    assert!(is_non_html_resource_url("file:///C:/Users/EDY/report.pdf"));
    assert!(is_non_html_resource_url("https://cdn.example.com/a.mp4"));
    // 查询串不影响扩展名识别
    assert!(is_non_html_resource_url(
        "https://cdn.example.com/a.png?v=2&size=large"
    ));
    // SVG 有 DOM 可脚本，按正常页面处理，不算资源页
    assert!(!is_non_html_resource_url("file:///C:/Users/EDY/icon.svg"));
    // HTML、无扩展名、非法 URL 不是资源页
    assert!(!is_non_html_resource_url("file:///C:/Users/EDY/page.html"));
    assert!(!is_non_html_resource_url("file:///C:/Users/EDY/noext"));
    assert!(!is_non_html_resource_url("not a url"));
}

#[test]
fn fetch_page_content_returns_success_for_resource_pages() {
    // 资源页导航完成即成功：不再等待正文脚本（对图片/PDF 必然超时），
    // 标题取文件名、正文留空
    let manager = BrowserManager::new();
    let url = "file:///C:/docs/pic.png";
    {
        let mut state = manager.state.lock().unwrap();
        state
            .navigation_signals
            .insert("tab-1".to_string(), navigation_signal(url));
    }
    let response = manager.fetch_page_content(
        url,
        1000,
        &NavigationTicket {
            tab_id: "tab-1".to_string(),
            navigation_id: 0,
        },
    );
    assert!(response.ok);
    assert_eq!(response.error, None);
    assert_eq!(response.title, "pic.png");
    assert_eq!(response.final_url, url);
    assert!(response.content.is_empty());
    // 编码文件名解码为原文展示（%E6%8A%A5%E8%A1%A8 → 报表）
    {
        let mut state = manager.state.lock().unwrap();
        state.navigation_signals.insert(
            "tab-2".to_string(),
            navigation_signal("file:///C:/docs/%E6%8A%A5%E8%A1%A8.pdf"),
        );
    }
    let response = manager.fetch_page_content(
        "file:///C:/docs/%E6%8A%A5%E8%A1%A8.pdf",
        1000,
        &NavigationTicket {
            tab_id: "tab-2".to_string(),
            navigation_id: 0,
        },
    );
    assert_eq!(response.title, "报表.pdf");
}

#[test]
fn resource_document_snapshot_is_complete_and_matching() {
    let snapshot = resource_document_snapshot("file:///C:/a.png", 7);
    assert_eq!(snapshot.document_id, "resource-7");
    assert_eq!(snapshot.ready_state, "complete");
    assert_eq!(snapshot.url, "file:///C:/a.png");
    assert_eq!(snapshot.title, "a.png");
    assert!(!snapshot.internal_error);
    assert!(snapshot.text.is_empty());
}

#[test]
fn normalize_navigation_url_converts_windows_drive_paths() {
    assert_eq!(
        normalize_navigation_url(r"C:\Users\foo\bar.png"),
        "file:///C:/Users/foo/bar.png"
    );
    assert_eq!(
        normalize_navigation_url("c:/users/foo/bar.png"),
        "file:///C:/users/foo/bar.png"
    );
    assert_eq!(
        normalize_navigation_url(r"d:\docs\mixed/slashes\x.pdf"),
        "file:///D:/docs/mixed/slashes/x.pdf"
    );
}

#[test]
fn normalize_navigation_url_encodes_local_path_specials() {
    assert_eq!(
        normalize_navigation_url(r"C:\docs\报表 图.pdf"),
        "file:///C:/docs/%E6%8A%A5%E8%A1%A8%20%E5%9B%BE.pdf"
    );
    assert_eq!(
        normalize_navigation_url(r"C:\tmp\a#b.png"),
        "file:///C:/tmp/a%23b.png"
    );
    assert_eq!(
        normalize_navigation_url(r"C:\tmp\a?b.png"),
        "file:///C:/tmp/a%3Fb.png"
    );
}

#[test]
fn normalize_navigation_url_converts_unix_and_unc_paths() {
    assert_eq!(
        normalize_navigation_url("/Users/foo/bar.png"),
        "file:///Users/foo/bar.png"
    );
    assert_eq!(
        normalize_navigation_url(r"\\server\share\doc.pdf"),
        "file://server/share/doc.pdf"
    );
    // 正斜杠 UNC 仅 Windows 按网络路径归一；Unix 视为本地路径剥多余斜杠
    #[cfg(windows)]
    assert_eq!(
        normalize_navigation_url("//server/share/doc.pdf"),
        "file://server/share/doc.pdf"
    );
    #[cfg(not(windows))]
    assert_eq!(
        normalize_navigation_url("//server/share/doc.pdf"),
        "file:///server/share/doc.pdf"
    );
}

#[test]
fn normalize_navigation_url_keeps_query_and_fragment_for_drive_urls() {
    // 盘符大写重拼时 query 与 fragment 必须一并带回（Unix 与网络路径
    // 本就走 Url::to_string 保留，仅盘符分支是手工重拼）
    assert_eq!(
        normalize_navigation_url("file:///c:/x/index.html?name=1#sec2"),
        "file:///C:/x/index.html?name=1#sec2"
    );
    assert_eq!(
        normalize_navigation_url("file:///C:/x/index.html#sec2"),
        "file:///C:/x/index.html#sec2"
    );
    assert_eq!(
        normalize_navigation_url("file:///c:/x/index.html?name=1"),
        "file:///C:/x/index.html?name=1"
    );
    // 参照：Unix 与网络路径形式原样保留
    assert_eq!(
        normalize_navigation_url("file:///Users/x/index.html?q=1#s2"),
        "file:///Users/x/index.html?q=1#s2"
    );
}

#[test]
fn normalize_navigation_url_percent_encodes_literal_percent() {
    // 字面 % 预编码为 %25，避免被按百分号解码误读（a%20b.png 是字面
    // 文件名而非已编码空格）
    assert_eq!(
        normalize_navigation_url(r"C:\tmp\a%20b.png"),
        "file:///C:/tmp/a%2520b.png"
    );
}

#[test]
fn normalize_navigation_url_normalizes_file_scheme_variants() {
    assert_eq!(
        normalize_navigation_url("file:C:/Users/x/a.png"),
        "file:///C:/Users/x/a.png"
    );
    assert_eq!(
        normalize_navigation_url("file://C:/Users/x/a.png"),
        "file:///C:/Users/x/a.png"
    );
    assert_eq!(
        normalize_navigation_url("file:///c:/Users/x/a.png"),
        "file:///C:/Users/x/a.png"
    );
    assert_eq!(
        normalize_navigation_url("file:/c:/Users/x/a.png"),
        "file:///C:/Users/x/a.png"
    );
    // UNC 形式（带主机名）保持结构
    assert_eq!(
        normalize_navigation_url("file://server/share/a.png"),
        "file://server/share/a.png"
    );
}

#[test]
fn normalize_navigation_url_is_idempotent_and_keeps_remote_urls() {
    assert_eq!(
        normalize_navigation_url("file:///C:/Users/x/a.png"),
        "file:///C:/Users/x/a.png"
    );
    assert_eq!(
        normalize_navigation_url("file:///Users/x/a.png"),
        "file:///Users/x/a.png"
    );
    assert_eq!(
        normalize_navigation_url("https://example.com/a"),
        "https://example.com/a"
    );
    assert_eq!(normalize_navigation_url("about:blank"), "about:blank");
    assert_eq!(normalize_navigation_url("example.com"), "example.com");
    assert_eq!(
        normalize_navigation_url("  C:\\a.png  "),
        "file:///C:/a.png"
    );
}

#[test]
fn ack_events_removes_only_injected_events() {
    let manager = BrowserManager::new();
    let first = BrowserEvent::NetworkResponse {
        timestamp: 1,
        url: "/api/a".to_string(),
        method: "POST".to_string(),
        status: 200,
        detail: "{}".to_string(),
    };
    let second = BrowserEvent::NetworkResponse {
        timestamp: 2,
        url: "/api/b".to_string(),
        method: "POST".to_string(),
        status: 200,
        detail: "{}".to_string(),
    };
    {
        let mut state = manager.state.lock().unwrap();
        state.pending_events.push(first.clone());
        state.pending_events.push(second.clone());
    }

    let removed = manager.ack_events(std::slice::from_ref(&first));

    assert_eq!(removed, 1);
    let state = manager.state.lock().unwrap();
    assert_eq!(state.pending_events, vec![second]);
}

fn tab(id: &str, url: &str, title: &str) -> BrowserTab {
    BrowserTab {
        id: id.to_string(),
        url: url.to_string(),
        title: title.to_string(),
        source: BrowserTabSource::User,
        agent_domain: None,
    }
}

#[test]
fn sync_tabs_by_id_preserves_records_when_only_metadata_differs() {
    // 模拟后端导航后 url/title 已更新，而前端传入的是旧元数据：
    // 按 id 同步时不应因 url/title 差异移除 tab 记录。
    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    state.tabs = vec![tab("t1", "https://real.example.com", "真实标题")];

    // 前端传入同 id 但 url/title 过时（仍是 about:blank）。
    BrowserManager::sync_tabs_by_id(&mut state, &[tab("t1", "about:blank", "")]);

    // tab 记录仍在，元数据被前端传入值覆盖。
    assert_eq!(state.tabs.len(), 1);
    assert_eq!(state.tabs[0].id, "t1");
    assert_eq!(state.tabs[0].url, "about:blank");
}

#[test]
fn sync_tabs_by_id_closes_records_for_removed_ids() {
    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    state.tabs = vec![
        tab("t1", "https://a.example.com", "A"),
        tab("t2", "https://b.example.com", "B"),
    ];

    // 前端仅保留 t1（t2 被显式关闭）。
    BrowserManager::sync_tabs_by_id(&mut state, &[tab("t1", "https://a.example.com", "A")]);

    let ids: Vec<&str> = state.tabs.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["t1"]);
}

#[test]
fn sync_tabs_by_id_adds_new_tab_records() {
    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    state.tabs = vec![tab("t1", "https://a.example.com", "A")];

    // 前端新增 t2（非 about: 链接，应补建 history）。
    BrowserManager::sync_tabs_by_id(
        &mut state,
        &[
            tab("t1", "https://a.example.com", "A"),
            tab("t2", "https://b.example.com", "B"),
        ],
    );

    assert_eq!(state.tabs.len(), 2);
    assert!(state.tab_histories.contains_key("t2"));
    assert!(state.navigation_signals.contains_key("t2"));
}

#[test]
fn sync_tabs_by_id_clears_metadata_for_removed_ids() {
    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    state.tabs = vec![tab("t1", "https://a.example.com", "A")];
    state.tab_histories.insert("t1".to_string(), Vec::new());
    state
        .navigation_signals
        .insert("t1".to_string(), navigation_signal("https://a.example.com"));

    // t1 被移除后其元数据应一并清理。
    BrowserManager::sync_tabs_by_id(&mut state, &[]);

    assert!(state.tabs.is_empty());
    assert!(!state.tab_histories.contains_key("t1"));
    assert!(!state.navigation_signals.contains_key("t1"));
}

#[test]
fn tab_history_keeps_repeated_visits_and_truncates_forward_branch() {
    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    state.tabs = vec![tab("t1", "about:blank", "")];
    state.active_tab_id = Some("t1".to_string());

    apply_tab_navigation_intent(
        &mut state,
        "t1",
        "https://example.com/a",
        NavigationIntent::Normal,
    )
    .unwrap();
    apply_tab_navigation_intent(
        &mut state,
        "t1",
        "https://example.com/b",
        NavigationIntent::Normal,
    )
    .unwrap();
    apply_tab_navigation_intent(
        &mut state,
        "t1",
        "https://example.com/a",
        NavigationIntent::Normal,
    )
    .unwrap();

    let urls: Vec<&str> = state.tab_histories["t1"]
        .iter()
        .map(|entry| entry.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![
            "https://example.com/a",
            "https://example.com/b",
            "https://example.com/a"
        ]
    );
    assert_eq!(state.tab_history_indices["t1"], 2);

    apply_tab_navigation_intent(
        &mut state,
        "t1",
        "https://example.com/b",
        NavigationIntent::History { target_index: 1 },
    )
    .unwrap();
    apply_tab_navigation_intent(
        &mut state,
        "t1",
        "https://example.com/d",
        NavigationIntent::Normal,
    )
    .unwrap();

    let urls: Vec<&str> = state.tab_histories["t1"]
        .iter()
        .map(|entry| entry.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![
            "https://example.com/a",
            "https://example.com/b",
            "https://example.com/d"
        ]
    );
    assert_eq!(state.tab_history_indices["t1"], 2);
}

#[test]
fn reload_retry_and_redirect_update_current_history_entry_only() {
    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    state.tabs = vec![tab("t1", "about:blank", "")];
    state.active_tab_id = Some("t1".to_string());
    let index = apply_tab_navigation_intent(
        &mut state,
        "t1",
        "https://example.com/start",
        NavigationIntent::Normal,
    )
    .unwrap();

    assert_eq!(
        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/start",
            NavigationIntent::Reload,
        )
        .unwrap(),
        index
    );
    assert_eq!(
        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/start",
            NavigationIntent::Retry,
        )
        .unwrap(),
        index
    );
    update_tab_navigation_entry(
        &mut state,
        "t1",
        index,
        "https://example.com/final",
        Some("最终页面"),
    );

    assert_eq!(state.tab_histories["t1"].len(), 1);
    assert_eq!(state.tab_history_indices["t1"], 0);
    assert_eq!(
        state.tab_histories["t1"][0].url,
        "https://example.com/final"
    );
    assert_eq!(state.tab_histories["t1"][0].title, "最终页面");
}

#[test]
fn agent_tabs_are_grouped_by_registrable_domain_and_never_match_user_tabs() {
    assert_eq!(
        agent_domain_for_url("https://docs.example.co.uk/a").unwrap(),
        "example.co.uk"
    );
    assert_eq!(
        agent_domain_for_url("https://api.example.co.uk/b").unwrap(),
        "example.co.uk"
    );
    assert_eq!(
        agent_domain_for_url("http://localhost:8080/a").unwrap(),
        "localhost"
    );

    let manager = BrowserManager::new();
    let mut state = manager.state.lock().unwrap();
    let mut user_tab = tab("user", "https://docs.example.com", "用户标签");
    user_tab.agent_domain = Some("example.com".to_string());
    let mut agent_tab = tab("agent", "https://api.example.com", "Agent 标签");
    agent_tab.source = BrowserTabSource::Agent;
    agent_tab.agent_domain = Some("example.com".to_string());
    state.tabs = vec![user_tab, agent_tab];

    assert_eq!(
        agent_tab_id_for_domain(&state, "example.com").as_deref(),
        Some("agent")
    );
    assert_eq!(agent_tab_id_for_domain(&state, "github.com"), None);
}

fn loading_navigation(requested_url: &str, navigation_id: u64) -> TabNavigationState {
    TabNavigationState {
        navigation_id,
        requested_url: requested_url.to_string(),
        started_url: None,
        document_id: None,
        superseded_document_ids: Vec::new(),
        final_url: None,
        history_index: Some(0),
        phase: NavigationPhase::Loading,
        internal_error_url: None,
    }
}

fn document_snapshot(document_id: &str, ready_state: &str, url: &str) -> WebDocumentSnapshot {
    WebDocumentSnapshot {
        document_id: document_id.to_string(),
        ready_state: ready_state.to_string(),
        url: url.to_string(),
        title: String::new(),
        text: String::new(),
        has_content: false,
        internal_error: false,
    }
}

#[test]
fn loading_navigation_rejects_superseded_document_and_accepts_redirect_to_same_url() {
    let mut navigation = loading_navigation("https://example.com/b", 2);
    navigation
        .superseded_document_ids
        .push("old-document-a".to_string());

    let stale = document_snapshot("old-document-a", "complete", "https://example.com/a");
    assert!(!accept_loading_document(&mut navigation, 2, &stale));
    assert!(navigation.document_id.is_none());

    let requested = document_snapshot("document-b", "loading", "https://example.com/b");
    assert!(accept_loading_document(&mut navigation, 2, &requested));

    let redirect = document_snapshot("new-document-a", "loading", "https://example.com/a");
    assert!(accept_loading_document(&mut navigation, 2, &redirect));
    assert_eq!(navigation.navigation_id, 2);
    assert_eq!(navigation.document_id.as_deref(), Some("new-document-a"));
    assert_eq!(
        navigation.started_url.as_deref(),
        Some("https://example.com/a")
    );
    assert!(navigation
        .superseded_document_ids
        .iter()
        .any(|document_id| document_id == "document-b"));

    let completed = document_snapshot("new-document-a", "complete", "https://example.com/a");
    assert!(accepts_completed_document(
        &navigation,
        2,
        "new-document-a",
        &completed,
    ));
    assert!(!accepts_completed_document(
        &navigation,
        2,
        "old-document-a",
        &completed,
    ));
}

#[test]
fn loading_navigation_does_not_supersede_repeated_current_document() {
    let mut navigation = loading_navigation("https://example.com/b", 2);
    let current = document_snapshot("document-b", "loading", "https://example.com/b");

    assert!(accept_loading_document(&mut navigation, 2, &current));
    assert!(accept_loading_document(&mut navigation, 2, &current));
    assert_eq!(navigation.document_id.as_deref(), Some("document-b"));
    assert!(!navigation
        .superseded_document_ids
        .iter()
        .any(|document_id| document_id == "document-b"));
}

#[test]
fn completion_requires_current_readable_document() {
    let mut navigation = loading_navigation("https://example.com/b", 2);
    navigation.started_url = Some("https://example.com/final".to_string());
    navigation.document_id = Some("document-b".to_string());

    let loading = document_snapshot("document-b", "interactive", "https://example.com/final");
    assert!(!accepts_completed_document(
        &navigation,
        2,
        "document-b",
        &loading,
    ));

    let mut interactive =
        document_snapshot("document-b", "interactive", "https://example.com/final");
    interactive.has_content = true;
    assert!(accepts_completed_document(
        &navigation,
        2,
        "document-b",
        &interactive,
    ));

    let complete = document_snapshot("document-b", "complete", "https://example.com/final");
    assert!(accepts_completed_document(
        &navigation,
        2,
        "document-b",
        &complete,
    ));
    assert!(!accepts_completed_document(
        &navigation,
        1,
        "document-b",
        &complete,
    ));
    assert!(!accepts_completed_document(
        &navigation,
        2,
        "document-a",
        &complete,
    ));
}
