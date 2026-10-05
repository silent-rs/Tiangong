//! 中继端到端：桌面端以自生成密钥接入、通道隔离、手机端消息透传、静态资源代理
//! 与单通道单桌面端约束。

use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tiangong_relay::protocol::{AgentToRelay, RelayToAgent};
use tiangong_relay::{RelayOptions, channel_id};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const SECRET: &str = "test-secret-0123456789";
const OTHER_SECRET: &str = "other-secret-0123456789";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn start(options: RelayOptions) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tiangong_relay::serve_with_listener(listener, options, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (format!("127.0.0.1:{}", addr.port()), tx)
}

async fn connect_agent(addr: &str, secret: &str) -> Ws {
    let mut request = format!("ws://{addr}/agent/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {secret}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn connect_client(addr: &str, channel: &str) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://{addr}/ws?c={channel}"))
        .await
        .unwrap()
        .0
}

async fn next_text(ws: &mut Ws) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .ok()??
        {
            Ok(Message::Text(text)) => return Some(text.to_string()),
            Ok(Message::Close(frame)) => {
                return Some(format!(
                    "close:{}",
                    frame.map(|f| u16::from(f.code)).unwrap_or(0)
                ));
            }
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

async fn next_agent_frame(ws: &mut Ws) -> RelayToAgent {
    let text = next_text(ws).await.expect("桌面端应收到帧");
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("帧格式无效：{text}"))
}

/// 一段时间内不应收到任何帧。
async fn expect_silence(ws: &mut Ws) {
    let result = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(result.is_err(), "不应收到帧：{result:?}");
}

async fn send_agent(ws: &mut Ws, frame: AgentToRelay) {
    ws.send(Message::text(serde_json::to_string(&frame).unwrap()))
        .await
        .unwrap();
}

#[tokio::test]
async fn relay_end_to_end() {
    let (addr, _shutdown) = start(RelayOptions::default()).await;
    let channel = channel_id(SECRET);

    // 过短的密钥被拒绝。
    let mut bad = connect_agent(&addr, "short").await;
    assert_eq!(next_text(&mut bad).await.as_deref(), Some("close:4003"));

    // 未带通道时给出引导页；通道桌面端未在线时页面返回 503。
    let anonymous = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(anonymous.status().as_u16(), 404);
    let offline = reqwest::get(format!("http://{addr}/?c={channel}"))
        .await
        .unwrap();
    assert_eq!(offline.status().as_u16(), 503);

    let mut agent = connect_agent(&addr, SECRET).await;
    assert_eq!(next_agent_frame(&mut agent).await, RelayToAgent::Welcome);

    // 同一通道同一时刻只接受一个桌面端。
    let mut second = connect_agent(&addr, SECRET).await;
    assert_eq!(next_text(&mut second).await.as_deref(), Some("close:4009"));

    // 健康检查按通道报告在线状态。
    let health: serde_json::Value = reqwest::get(format!("http://{addr}/healthz?c={channel}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["agent_online"], true);
    assert_eq!(health["agents"], 1);

    // 首次带通道参数访问：资源经桌面端提供，并写入通道 Cookie。
    let fetch = tokio::spawn({
        let addr = addr.clone();
        let channel = channel.clone();
        async move {
            let response = reqwest::get(format!("http://{addr}/?c={channel}"))
                .await
                .unwrap();
            let cookie = response
                .headers()
                .get("set-cookie")
                .map(|value| value.to_str().unwrap().to_string());
            (response.status().as_u16(), cookie)
        }
    });
    let RelayToAgent::AssetReq { req, path, .. } = next_agent_frame(&mut agent).await else {
        panic!("应收到资源请求");
    };
    assert_eq!(path, "");
    send_agent(
        &mut agent,
        AgentToRelay::AssetRes {
            req,
            status: 200,
            mime: "text/html".into(),
            body: base64::engine::general_purpose::STANDARD.encode("<html></html>"),
            no_store: true,
            sandbox: false,
        },
    )
    .await;
    let (status, cookie) = fetch.await.unwrap();
    assert_eq!(status, 200);
    assert!(
        cookie
            .unwrap()
            .starts_with(&format!("tg_channel={channel}"))
    );

    // 之后的绝对路径资源请求靠 Cookie 路由到同一通道。
    let fetch = tokio::spawn({
        let addr = addr.clone();
        let channel = channel.clone();
        async move {
            let response = reqwest::Client::new()
                .get(format!("http://{addr}/assets/a.js?v=1"))
                .header("cookie", format!("tg_channel={channel}"))
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let mime = response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let csp = response.headers().get("content-security-policy").is_some();
            (status, mime, csp, response.text().await.unwrap())
        }
    });
    let RelayToAgent::AssetReq { req, path, query } = next_agent_frame(&mut agent).await else {
        panic!("应收到资源请求");
    };
    assert_eq!(path, "assets/a.js");
    assert_eq!(query, "v=1");
    send_agent(
        &mut agent,
        AgentToRelay::AssetRes {
            req,
            status: 200,
            mime: "text/javascript".into(),
            body: base64::engine::general_purpose::STANDARD.encode("ok()"),
            no_store: false,
            sandbox: true,
        },
    )
    .await;
    let (status, mime, csp, body) = fetch.await.unwrap();
    assert_eq!(
        (status, mime.as_str(), csp, body.as_str()),
        (200, "text/javascript", true, "ok()")
    );

    // 手机端消息双向透传。
    let mut client = connect_client(&addr, &channel).await;
    let RelayToAgent::ClientOpen { conn } = next_agent_frame(&mut agent).await else {
        panic!("应收到连接建立");
    };
    client
        .send(Message::text(r#"{"t":"hello"}"#))
        .await
        .unwrap();
    assert_eq!(
        next_agent_frame(&mut agent).await,
        RelayToAgent::ClientMsg {
            conn: conn.clone(),
            data: r#"{"t":"hello"}"#.into()
        }
    );
    send_agent(
        &mut agent,
        AgentToRelay::ToClient {
            conn: conn.clone(),
            data: r#"{"t":"ready"}"#.into(),
        },
    )
    .await;
    assert_eq!(
        next_text(&mut client).await.as_deref(),
        Some(r#"{"t":"ready"}"#)
    );

    // 桌面端可关闭手机端连接。
    send_agent(
        &mut agent,
        AgentToRelay::CloseClient {
            conn: conn.clone(),
            reason: "denied".into(),
        },
    )
    .await;
    assert_eq!(next_text(&mut client).await.as_deref(), Some("close:4000"));
    // 被关闭后，该连接后续消息不再转发给桌面端。
    let _ = client.send(Message::text(r#"{"t":"invoke"}"#)).await;

    // 手机端主动断开时桌面端收到通知。
    let mut client3 = connect_client(&addr, &channel).await;
    let RelayToAgent::ClientOpen { conn: conn3 } = next_agent_frame(&mut agent).await else {
        panic!("应收到连接建立");
    };
    client3.close(None).await.unwrap();
    while client3.next().await.is_some() {}
    assert_eq!(
        next_agent_frame(&mut agent).await,
        RelayToAgent::ClientClose { conn: conn3 }
    );

    // 桌面端断开后，在线手机端被告知离线。
    let mut client2 = connect_client(&addr, &channel).await;
    assert!(matches!(
        next_agent_frame(&mut agent).await,
        RelayToAgent::ClientOpen { .. }
    ));
    agent.close(None).await.unwrap();
    assert_eq!(next_text(&mut client2).await.as_deref(), Some("close:4001"));

    // 桌面端离线时新的手机端连接直接被告知离线。
    let mut late = connect_client(&addr, &channel).await;
    assert_eq!(next_text(&mut late).await.as_deref(), Some("close:4001"));

    // 断开后新的桌面端可以接入。
    let mut agent2 = connect_agent(&addr, SECRET).await;
    assert_eq!(next_agent_frame(&mut agent2).await, RelayToAgent::Welcome);
}

/// 多个桌面端共用一个中继：各自通道互不可见。
#[tokio::test]
async fn channels_are_isolated() {
    let (addr, _shutdown) = start(RelayOptions { max_agents: 2 }).await;
    let mut agent_a = connect_agent(&addr, SECRET).await;
    assert_eq!(next_agent_frame(&mut agent_a).await, RelayToAgent::Welcome);
    let mut agent_b = connect_agent(&addr, OTHER_SECRET).await;
    assert_eq!(next_agent_frame(&mut agent_b).await, RelayToAgent::Welcome);

    // 接入数达到上限。
    let mut third = connect_agent(&addr, "third-secret-0123456789").await;
    assert_eq!(next_text(&mut third).await.as_deref(), Some("close:4029"));

    // 手机端只连到自己通道的桌面端。
    let mut phone_a = connect_client(&addr, &channel_id(SECRET)).await;
    let RelayToAgent::ClientOpen { conn: conn_a } = next_agent_frame(&mut agent_a).await else {
        panic!("A 应收到连接建立");
    };
    expect_silence(&mut agent_b).await;

    // B 不能向 A 通道的手机端发消息或关闭其连接。
    send_agent(
        &mut agent_b,
        AgentToRelay::ToClient {
            conn: conn_a.clone(),
            data: r#"{"t":"spoof"}"#.into(),
        },
    )
    .await;
    send_agent(
        &mut agent_b,
        AgentToRelay::CloseClient {
            conn: conn_a.clone(),
            reason: "spoof".into(),
        },
    )
    .await;
    expect_silence(&mut phone_a).await;

    // A 正常通信。
    send_agent(
        &mut agent_a,
        AgentToRelay::ToClient {
            conn: conn_a,
            data: r#"{"t":"ready"}"#.into(),
        },
    )
    .await;
    assert_eq!(
        next_text(&mut phone_a).await.as_deref(),
        Some(r#"{"t":"ready"}"#)
    );

    // B 断开不影响 A 的手机端。
    agent_b.close(None).await.unwrap();
    expect_silence(&mut phone_a).await;
}

fn detect_lan_ip() -> Option<std::net::IpAddr> {
    // 与桌面端同口径：取已启用、非点对点网卡上的私有 IPv4（排除代理 TUN）。
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|iface| iface.is_oper_up() && !iface.is_p2p() && !iface.is_loopback())
        .map(|iface| iface.ip())
        .find(|ip| match ip {
            std::net::IpAddr::V4(v4) => v4.is_private(),
            std::net::IpAddr::V6(_) => false,
        })
}

/// 局域网直连形态：中继监听所有网卡，桌面端经回环接入，手机端经局域网 IP 访问。
#[tokio::test]
async fn lan_direct_mode() {
    let Some(lan_ip) = detect_lan_ip() else {
        eprintln!("未探测到局域网地址，跳过");
        return;
    };
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_shutdown, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tiangong_relay::serve_with_listener(listener, RelayOptions { max_agents: 1 }, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut agent = connect_agent(&format!("127.0.0.1:{port}"), SECRET).await;
    assert_eq!(next_agent_frame(&mut agent).await, RelayToAgent::Welcome);
    let channel = channel_id(SECRET);

    let lan = format!("{lan_ip}:{port}");
    // 直连局域网地址：不走系统代理（代理会把私网请求转发出去），并设置短超时。
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let health: serde_json::Value = match client
        .get(format!("http://{lan}/healthz?c={channel}"))
        .send()
        .await
    {
        Ok(response) => response.json().await.unwrap(),
        Err(error) => {
            // 部分受限环境（如沙箱化终端）会拦截本地编译程序的局域网入站，
            // 与中继实现无关；此时跳过，真机局域网验证见 docs/remote-access.md。
            eprintln!("局域网地址 {lan} 不可达（{error}），当前环境限制局域网入站，跳过");
            return;
        }
    };
    assert_eq!(health["agent_online"], true);

    let (mut phone, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(format!("ws://{lan}/ws?c={channel}")),
    )
    .await
    .expect("局域网 WebSocket 连接超时")
    .unwrap();
    let RelayToAgent::ClientOpen { conn } = next_agent_frame(&mut agent).await else {
        panic!("应收到连接建立");
    };
    phone.send(Message::text(r#"{"t":"hello"}"#)).await.unwrap();
    assert!(matches!(
        next_agent_frame(&mut agent).await,
        RelayToAgent::ClientMsg { conn: c, .. } if c == conn
    ));
    send_agent(
        &mut agent,
        AgentToRelay::ToClient {
            conn,
            data: r#"{"t":"ready"}"#.into(),
        },
    )
    .await;
    assert_eq!(
        next_text(&mut phone).await.as_deref(),
        Some(r#"{"t":"ready"}"#)
    );
}
