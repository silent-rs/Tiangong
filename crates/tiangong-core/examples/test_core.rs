use std::sync::mpsc;
use tiangong_core::agent_input::{AgentInput, AgentInputKind};
use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
use tiangong_core::core::TiangongCore;
use tiangong_core::session::Session;
use tiangong_types::StreamEvent;

fn main() {
    // 示例：使用默认配置（第三方开发者可直接构造 CoreConfig）
    let config = CoreConfigProvider::new(CoreConfig::default());
    let (tx, rx) = mpsc::channel::<StreamEvent>();
    // storage_root 必须由调用方提供（core 不自行计算路径）。
    // 这里用 home 目录下的 .tiangong 作为示例；生产入口由 tiangong-app-state 注入。
    let storage_root = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".tiangong");
    let mut session = Session::new("测试");
    session.bind_storage_root(storage_root.clone());
    session
        .try_persist_to_disk()
        .expect("初始 Session 应保存成功");
    // Core 只持有会话 ID，实际 Session 在每轮开始时从存储中加载。
    let core = TiangongCore::builder()
        .session_id(session.id.clone())
        .config(config)
        .trust_mode(session.trust_mode)
        .storage_root(storage_root)
        .workspace_dir(
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        )
        .stream_tx(tx)
        .plugins(Vec::new())
        // Core 构造即需持有实际模型（宿主解析模型注册表后传入）。
        .model_endpoint(tiangong_llm::ModelEndpoint {
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-example".to_string(),
            model: "example-model".to_string(),
            ..Default::default()
        })
        .build();

    println!("=== 发送: 你好 ===");
    let _ = core.deliver(AgentInputKind::message("你好"));

    let mut got_done = false;
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(se) => match &se {
                StreamEvent::UserMessage { content, .. } => println!("[用户] {content}"),
                StreamEvent::Delta { content: text, .. } => print!("{text}"),
                StreamEvent::Reasoning { .. } => print!("[R]"),
                StreamEvent::ToolCalls { names, .. } => println!("\n[调用] {}", names.join(",")),
                StreamEvent::ToolStart { name, .. } => println!("[开始] {name}"),
                StreamEvent::ToolResult { name, ok, .. } => println!("[结果] {name} ok={ok}"),
                StreamEvent::Done { .. } => {
                    println!("\n=== Done ===");
                    got_done = true;
                    break;
                }
                StreamEvent::Error { message: err } => {
                    println!("\n=== Error: {err} ===");
                    break;
                }
                _ => {}
            },
            Err(_) => {
                println!("\n=== 超时 ===");
                break;
            }
        }
    }

    println!("got_done={got_done}");

    if got_done {
        println!("\n=== 发送: 1+1=? ===");
        let _ = core.deliver(AgentInputKind::message("1+1=?"));

        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(se) => match &se {
                    StreamEvent::Delta { content: text, .. } => print!("{text}"),
                    StreamEvent::Reasoning { .. } => print!("[R]"),
                    StreamEvent::Done { .. } => {
                        println!("\n=== 第二轮 Done ===");
                        break;
                    }
                    StreamEvent::Error { message: err } => {
                        println!("\n=== 第二轮 Error: {err} ===");
                        break;
                    }
                    _ => {}
                },
                Err(_) => {
                    println!("\n=== 第二轮超时 ===");
                    break;
                }
            }
        }
    }

    println!("\n=== shutdown ===");
    let session = core.into_session().expect("worker 应正常退出");
    println!("消息数: {}", session.messages.len());
    for (i, m) in session.messages.iter().enumerate() {
        let text: String = m
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .collect::<Vec<_>>()
            .join("");
        let preview: String = text.chars().take(80).collect();
        println!(
            "[{i}] {:?} clen={} | {}",
            m.role,
            text.len(),
            preview.replace('\n', "\\n")
        );
    }
}
