use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;
use tiangong_memory::{
    Episode, EpisodeOutcome, LeaderState, ManagedMemory, MemoryHandle, MemoryOptions,
    MemoryVectorMode, ProcessType, RecallAnchors, memory_service_name, read_leader_info,
    start_or_connect_with_options,
};

const MULTIPROCESS_CHILD_ROLE_ENV: &str = "TIANGONG_MEMORY_MULTIPROCESS_CHILD_ROLE";
const MULTIPROCESS_CHILD_WORKSPACE_ENV: &str = "TIANGONG_MEMORY_MULTIPROCESS_CHILD_WORKSPACE";
const MULTIPROCESS_READY_PREFIX: &str = "TIANGONG_MEMORY_CHILD_READY";

struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_userprofile: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn enter(home: &std::path::Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
        }
        Self {
            prev_home,
            prev_userprofile,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn follower_continues_memory_after_leader_disappears() {
    let home = TempDir::new().expect("创建 fake home 失败");
    let _env = EnvGuard::enter(home.path());
    let workspace_id = "ws-auto-continuation".to_string();

    log_step(format!(
        "测试开始：home={} workspace={workspace_id}",
        home.path().display()
    ));

    let leader = start_or_connect_test(ProcessType::Cli)
        .await
        .expect("启动 leader 失败");
    log_state("leader 启动后", &leader);
    log_leader_info("leader 启动后");
    assert!(leader.is_leader(), "首个 memory 实例应成为 leader");

    let follower = start_or_connect_test(ProcessType::Server)
        .await
        .expect("启动 follower 失败");
    log_state("follower 启动后", &follower);
    log_leader_info("follower 连接后");
    assert!(
        matches!(follower.state(), LeaderState::Follower { .. }),
        "第二个 memory 实例应通过 TCP 连接 leader"
    );

    log_step("通过 follower 写入接替前 Episode");
    follower.handle().write_episode(
        make_episode(
            "session-before-failover",
            "persist before failover",
            "persist before failover through follower",
            vec!["before".to_string(), "failover".to_string()],
        ),
        Some(workspace_id.clone()),
    );
    assert_recall_contains(&follower, "before failover", "persist before failover").await;

    log_step("drop leader，模拟 leader 进程退出");
    drop(leader);
    wait_until_leader(&follower).await;
    log_state("follower 自动接替后", &follower);
    log_leader_info("follower 自动接替后");

    log_step("验证接替前写入的 Episode 仍可召回");
    assert_recall_contains(&follower, "before failover", "persist before failover").await;

    log_step("通过晋升后的 follower 写入接替后 Episode");
    follower.handle().write_episode(
        make_episode(
            "session-after-failover",
            "write after failover",
            "write after failover using promoted follower",
            vec!["after".to_string(), "failover".to_string()],
        ),
        Some(workspace_id),
    );
    assert_recall_contains(&follower, "after failover", "write after failover").await;
    log_step("测试完成：自动接续链路验证通过");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn gui_cli_server_processes_share_one_workspace_leader() {
    let home = TempDir::new().expect("创建 fake home 失败");
    let _env = EnvGuard::enter(home.path());
    let workspace_id = format!("ws-multiprocess-{}", scru128::new());

    log_step(format!(
        "多进程验证开始：home={} workspace={workspace_id}",
        home.path().display()
    ));

    let mut children = Vec::new();
    let mut ready_states = Vec::new();
    for role in ["cli", "gui", "server"] {
        let (child, ready) = spawn_multiprocess_child(home.path(), &workspace_id, role);
        log_step(format!(
            "子进程就绪：role={} state={} title={}",
            ready.role, ready.state, ready.title
        ));
        children.push(child);
        ready_states.push(ready);
    }

    let leader_count = ready_states
        .iter()
        .filter(|ready| ready.state == "leader")
        .count();
    assert_eq!(
        leader_count, 1,
        "GUI + CLI + Server 同一 workspace 只能产生一个 leader"
    );
    assert!(
        ready_states.iter().any(|ready| ready.state == "follower"),
        "至少一个入口应作为 follower 连接已有 leader"
    );

    let service = memory_service_name();
    let remote_handle = MemoryHandle::connect_tcp(&service)
        .await
        .expect("父进程应能连接多进程 leader");

    for ready in &ready_states {
        assert_handle_recall_contains(
            &remote_handle,
            &format!("multiprocess {}", ready.role),
            &ready.title,
        )
        .await;
    }

    drop(children);
    log_step("多进程验证完成：三个入口共享同一 workspace leader 且写入可召回");
}

#[tokio::test(flavor = "current_thread")]
async fn multiprocess_memory_actor_child_entry() {
    let Ok(role) = std::env::var(MULTIPROCESS_CHILD_ROLE_ENV) else {
        return;
    };
    let workspace_id =
        std::env::var(MULTIPROCESS_CHILD_WORKSPACE_ENV).expect("子进程缺少 workspace 环境变量");
    let process_type = process_type_for_role(&role);
    let memory = start_or_connect_test(process_type)
        .await
        .expect("子进程启动或连接 Memory 失败");

    let state = if memory.is_leader() {
        "leader"
    } else {
        "follower"
    };
    let title = format!("multiprocess {role} memory write");
    memory.handle().write_episode(
        make_episode(
            &format!("session-multiprocess-{role}"),
            &title,
            &format!("multiprocess {role} writes through {state} memory handle"),
            vec!["multiprocess".to_string(), role.clone(), state.to_string()],
        ),
        Some(workspace_id),
    );

    println!("{MULTIPROCESS_READY_PREFIX} role={role} state={state} title={title}");
    std::io::stdout().flush().expect("刷新子进程输出失败");
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// 以「向量层禁用」的 MemoryOptions 执行选举，与 registry / election 单测保持一致。
///
/// failover 集成测试只验证 leader/follower 自动接续，不涉及向量检索。
/// 用 `Disabled` 跳过 lancedb（C++ FFI）初始化，从根上消除测试进程 teardown
/// 时 Actor 线程析构 lancedb 与进程 C++ 静态析构的竞态（Linux 偶发 SIGSEGV）。
async fn start_or_connect_test(process_type: ProcessType) -> anyhow::Result<ManagedMemory> {
    let options = MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled);
    start_or_connect_with_options(options, process_type).await
}

fn make_episode(session_id: &str, title: &str, summary: &str, keywords: Vec<String>) -> Episode {
    Episode::new(
        session_id.to_string(),
        title.to_string(),
        summary.to_string(),
        EpisodeOutcome::Success,
        keywords,
        vec!["memory_failover_integration".to_string()],
        0.9,
    )
}

#[derive(Debug)]
struct ChildReady {
    role: String,
    state: String,
    title: String,
}

struct MultiprocessChild {
    child: Child,
}

impl Drop for MultiprocessChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_multiprocess_child(
    home: &Path,
    workspace_id: &str,
    role: &str,
) -> (MultiprocessChild, ChildReady) {
    let exe = std::env::current_exe().expect("读取当前测试二进制失败");
    let mut child = Command::new(exe)
        .arg("--exact")
        .arg("multiprocess_memory_actor_child_entry")
        .arg("--nocapture")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env(MULTIPROCESS_CHILD_ROLE_ENV, role)
        .env(MULTIPROCESS_CHILD_WORKSPACE_ENV, workspace_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|err| panic!("启动 {role} 子进程失败: {err}"));

    let stdout = child.stdout.take().expect("子进程 stdout 未开启");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if line.contains(MULTIPROCESS_READY_PREFIX) => {
                    let _ = tx.send(parse_child_ready_line(&line));
                    return;
                }
                Ok(_) => {}
                Err(err) => {
                    let _ = tx.send(Err(format!("读取子进程输出失败: {err}")));
                    return;
                }
            }
        }
        let _ = tx.send(Err("子进程退出前未输出 ready 状态".to_string()));
    });

    let ready = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(ready)) => ready,
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{role} 子进程 ready 失败: {err}");
        }
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{role} 子进程 ready 超时: {err}");
        }
    };
    (MultiprocessChild { child }, ready)
}

fn parse_child_ready_line(line: &str) -> Result<ChildReady, String> {
    let start = line
        .find(MULTIPROCESS_READY_PREFIX)
        .ok_or_else(|| format!("ready 行缺少前缀: {line}"))?;
    let fields = line[start + MULTIPROCESS_READY_PREFIX.len()..].trim();
    let role = parse_ready_field(fields, "role")?;
    let state = parse_ready_field(fields, "state")?;
    let title = fields
        .split_once("title=")
        .map(|(_, title)| title.trim().to_string())
        .filter(|title| !title.is_empty())
        .ok_or_else(|| format!("ready 行缺少 title: {line}"))?;
    Ok(ChildReady { role, state, title })
}

fn parse_ready_field(fields: &str, key: &str) -> Result<String, String> {
    fields
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{key}=")))
        .map(ToString::to_string)
        .ok_or_else(|| format!("ready 行缺少 {key}: {fields}"))
}

fn process_type_for_role(role: &str) -> ProcessType {
    match role {
        "cli" => ProcessType::Cli,
        "gui" => ProcessType::Gui,
        "server" => ProcessType::Server,
        other => panic!("未知多进程角色: {other}"),
    }
}

async fn wait_until_leader(memory: &ManagedMemory) {
    for attempt in 0..40 {
        if memory.is_leader() {
            log_step(format!(
                "follower 已在第 {} 次检查时晋升为 leader",
                attempt + 1
            ));
            return;
        }
        if attempt % 5 == 0 {
            log_step(format!(
                "等待 follower 自动接替：attempt={} state={:?}",
                attempt + 1,
                memory.state()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("follower 未在预期时间内自动接替为 leader");
}

async fn assert_recall_contains(memory: &ManagedMemory, query: &str, expected_title: &str) {
    for attempt in 0..30 {
        let hits = memory
            .handle()
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                5,
            )
            .await;
        if attempt == 0 || !hits.is_empty() {
            let titles = hits
                .iter()
                .map(|hit| format!("{}({:.3})", hit.title, hit.score))
                .collect::<Vec<_>>()
                .join(", ");
            log_step(format!(
                "召回检查：query={query} attempt={} hits=[{}]",
                attempt + 1,
                titles
            ));
        }
        if hits.iter().any(|hit| hit.title.contains(expected_title)) {
            log_step(format!("召回命中预期记忆：title={expected_title}"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("未召回预期记忆：query={query}, title={expected_title}");
}

async fn assert_handle_recall_contains(handle: &MemoryHandle, query: &str, expected_title: &str) {
    for attempt in 0..30 {
        let hits = handle
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                8,
            )
            .await;
        if attempt == 0 || !hits.is_empty() {
            let titles = hits
                .iter()
                .map(|hit| format!("{}({:.3})", hit.title, hit.score))
                .collect::<Vec<_>>()
                .join(", ");
            log_step(format!(
                "父进程召回检查：query={query} attempt={} hits=[{}]",
                attempt + 1,
                titles
            ));
        }
        if hits.iter().any(|hit| hit.title.contains(expected_title)) {
            log_step(format!("父进程召回命中预期记忆：title={expected_title}"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("父进程未召回预期记忆：query={query}, title={expected_title}");
}

fn log_step(message: impl AsRef<str>) {
    eprintln!("[memory-failover-it] {}", message.as_ref());
}

fn log_state(label: &str, memory: &ManagedMemory) {
    log_step(format!("{label}: state={:?}", memory.state()));
}

fn log_leader_info(label: &str) {
    match read_leader_info() {
        Ok(Some(info)) => log_step(format!(
            "{label}: leader pid={} process={:?} service={} heartbeat={}",
            info.pid, info.process_type, info.service, info.heartbeat_at
        )),
        Ok(None) => log_step(format!("{label}: leader 信息不存在")),
        Err(err) => log_step(format!("{label}: 读取 leader 信息失败: {err}")),
    }
}
