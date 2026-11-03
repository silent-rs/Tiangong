//! Agent 命令执行：包装脚本执行与夭折/交互/超时兜底收尾。
//!
//! 执行前的 shell 就绪握手与前台探测见 [`super::readiness`]。

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::protocol::{
    CommandMarkers, ParsedCommandOutput, command_from_request, parse_command_output,
    prepare_non_interactive_command,
};
use super::readiness::Foreground;
use super::shell::{shell_ready_probe, shell_ready_probe_completed};
use super::types::{ExecRequest, ExecResponse};
use super::{COMMAND_POLL_INTERVAL_MS, SessionEndState, SessionPhase, TerminalService};

/// `run_command` 的单次等待反馈预算默认值（秒，issue #697 续等协议）：
/// 命令未在此窗口内完成**不中断**，登记为可续等命令并返回 still_running，
/// Agent 从输出内容自行识别状况（等待输入、交互程序或仍在执行），用
/// terminal(action=send, input="") 续等收割、status 查询或 \x03 中断。
/// 短命令（绝大多数）窗口内直接完成，一次调用拿全结果。
const EXEC_DEFAULT_WAIT_SECS: u64 = 5;

/// `timeout` 参数的上限：与 terminal_send 的等待上限对齐，须小于宿主
/// 工具时限，保证如实返回 still_running 而不是被宿主按超时取消。
pub(super) const EXEC_MAX_WAIT_SECS: u64 = 110;

/// 包装脚本夭折的兜底判定：start marker 已出现、退出码标记还没出现、
/// 输出静默达本时长后，才开始探测「登录 shell 是否已回到提示符」。
///
/// 这里守的**不是** issue #571 的原始场景——用户命令已隔离进内层脚本
/// （见 `protocol::prepare_posix_command`），解析期错误只终止内层 `source`，
/// wrapper 照常输出 cwd/rc/end marker，那类命令不会走到这里。
///
/// 本兜底覆盖的是剩下两类：
/// 1. **shell 仍存活、但 marker 永远不会来**。shell 随命令一起死亡由
///    [`SessionEndState::Ended`] 分支接管；此处专收 shell 被顶替/替换后
///    新 shell 还活着的情形（如内层 `exec sh -i`），以及 marker 被前台
///    程序吞掉、shell 插件干扰等无法穷举的意外。
/// 2. **带显式 timeout 的命令**。交互程序兜底分支带 `deadline.is_none()`
///    前置条件，命令一旦设了 timeout 就完全不生效；本分支不受该约束，
///    是 timeout 路径下唯一能提前收尾的兜底。否则长 timeout 命令一旦丢
///    marker，就要轮询满整个超时，且终端会被标记 Unresponsive 退出复用池。
///
/// 因此即便 #571 已根治，本兜底仍不可移除。
const SILENT_ABORTED_WRAPPER_SECS: u64 = 3;

/// 两次夭折探测的最小间隔：探测要往 shell 输入缓冲写一行，不能每轮都写。
const ABORTED_WRAPPER_PROBE_INTERVAL_SECS: u64 = 5;

/// 夭折探测的最大次数。连续未被执行说明前台确实还在消费 stdin，
/// 继续写入只会不断污染它的输入，到达上限后回到纯等待。
const MAX_ABORTED_WRAPPER_PROBES: u32 = 3;

/// 单次夭折探测等待就绪 marker 的时间预算。
const ABORTED_WRAPPER_PROBE_TIMEOUT_MS: u64 = 800;

impl TerminalService {
    /// 命令使 shell 本身退出时的收尾：已解析出的命令退出码优先于
    /// shell 退出码（结束标记先于 shell 退出的组合场景）；已收集输出
    /// 照常返回，并回收死亡会话。
    fn ended_session_response(
        &self,
        session_id: &str,
        start_offset: u64,
        markers: &CommandMarkers,
    ) -> Result<ExecResponse> {
        let shell_exit = match self.session_end_state(session_id) {
            SessionEndState::Ended(code) => Some(code),
            SessionEndState::Alive => None,
            SessionEndState::Reaped => {
                bail!("终端会话已被关闭或回收，命令结果不可恢复：{session_id}")
            }
        };
        let raw = self.raw_output_since(session_id, start_offset)?;
        let parsed = parse_command_output(&raw, markers);
        let parsed_exit_code = parsed.exit_code;
        let mut response = parsed.into_response();
        response.exit_code = parsed_exit_code.or(shell_exit).unwrap_or(0);
        response.session_ended = true;
        if shell_exit.is_some() {
            self.reap_ended_session(session_id);
        }
        Ok(response)
    }

    pub(super) async fn exec_in_session(&self, request: ExecRequest) -> Result<ExecResponse> {
        let shell = self.with_session(&request.session_id, |session| Ok(session.shell))?;
        let command = command_from_request(&request, shell)?;
        let exec_lock = self.with_session(&request.session_id, |session| {
            Ok(Arc::clone(&session.exec_lock))
        })?;
        let _guard = exec_lock.lock().await;
        self.with_session(&request.session_id, |session| {
            if session.phase != SessionPhase::Idle && session.phase != SessionPhase::Reserved {
                // 执行中/未就绪的终端不接受新命令：防止包装脚本混进
                // 正在运行的命令输出或未收尾的 shell。
                bail!("终端正忙（上条命令未完成或就绪确认未通过），可换终端或不指定 terminal_id 自动选择");
            }
            session.phase = SessionPhase::Running;
            session.running_command = Some(command.clone());
            Ok(())
        })?;

        let result = async {
            // 先取消 shell 中的残留输入或前台命令，再用隐藏探针确认登录
            // shell 已重新接管，最后才发送 Agent 的真实命令。
            self.prepare_shell_for_command(&request.session_id).await?;
            self.exec_non_interactive(&request, &command).await
        }
        .await;
        let next_phase = if result.as_ref().is_ok_and(|response| response.still_running) {
            // 续等交还：命令仍在跑且已登记 pending_send，保持 Running
            //（不被选终端复用），由续等调用或取消路径收尾。
            SessionPhase::Running
        } else {
            SessionPhase::Idle
        };
        let _ = self.with_session(&request.session_id, |session| {
            // 就绪确认失败的终端保持 Unresponsive：终端仍存活可用（后台
            // 作业、用户输入都在），但不得被选终端自动复用，否则下一次
            // 调用又会撞在同一个未回到提示符的 shell 上。
            if session.phase != SessionPhase::Unresponsive {
                session.phase = next_phase;
                // 回到空闲的命令已结束；交互中的程序名对查询仍有价值，
                // 与 Unresponsive（收尾中的命令）一并保留。
                if next_phase == SessionPhase::Idle {
                    session.running_command = None;
                }
            }
            Ok(())
        });
        result
    }

    /// 单次等待预算耗尽：把仍在执行的包装命令登记为待确认命令（带边界
    /// 标记），交还控制权（issue #697 续等协议）。命令**不被中断**：续等
    /// 经 terminal(action=send, input="") 走 marker 协议精确收割完整输出
    /// 与退出码；pending 存在期间新的 enter=true 命令被拒绝，选终端跳过
    /// 本终端——与交互会话的既有保护一致。
    fn hand_over_running_command(
        &self,
        session_id: &str,
        command: &str,
        command_started_at: std::time::Instant,
        start_offset: u64,
        markers: &CommandMarkers,
        budget_secs: u64,
    ) -> ExecResponse {
        // 截至目前的解析结果（输出重算一次，忽略环形缓冲截断的极端情形）。
        let parsed = self
            .raw_output_since(session_id, start_offset)
            .map(|raw| parse_command_output(&raw, markers))
            .unwrap_or_default();
        // 会话已在等待中死亡：end marker 不会再出现，按死亡收尾。
        if matches!(
            self.session_end_state(session_id),
            SessionEndState::Ended(_)
        ) {
            return self
                .ended_session_response(session_id, start_offset, markers)
                .unwrap_or_else(|error| {
                    let mut response = parsed.into_response();
                    response.stderr = format!("命令交还失败：{error:#}");
                    response.exit_code = -1;
                    response
                });
        }
        let registered = self.with_session(session_id, |session| {
            session.pending_send = Some(super::input::PendingSend::for_exec_handover(
                command.trim().to_string(),
                command_started_at,
                start_offset,
                markers.clone(),
            ));
            Ok(())
        });
        if registered.is_err() {
            let mut response = parsed.into_response();
            response.stderr = "命令仍在执行，但续等登记失败（终端已不可用）".to_string();
            response.exit_code = -1;
            return response;
        }
        let mut response = parsed.into_response();
        response.still_running = true;
        response.stderr = format!(
            "命令未在单次等待窗口（{} 秒）内完成，未中断；继续等待并收割完整输出与退出码用 terminal（action=send，terminal_id 对应终端，input 留空），\
            预期耗时更长的命令可显式设置更长的 timeout（秒，上限 {}），需要中断则发送 \\x03",
            budget_secs, EXEC_MAX_WAIT_SECS,
        );
        response
    }

    async fn exec_non_interactive(
        &self,
        request: &ExecRequest,
        command: &str,
    ) -> Result<ExecResponse> {
        let marker_id = scru128::new().to_string();
        let markers = CommandMarkers::new(&marker_id);
        let shell = self.with_session(&request.session_id, |session| Ok(session.shell))?;
        let prepared =
            prepare_non_interactive_command(command, request.cwd.as_deref(), &markers, shell)?;
        let start_offset =
            self.with_session(&request.session_id, |session| Ok(session.raw_bytes_total))?;

        // 命令回显交给输出过滤器：它在 start marker 到达时把 marker 行
        // 替换成命令文本。此处若直接推给 UI，命令会先于 shell 为就绪
        // 探针重绘的提示符落地，用户看到的就是「命令、提示符、输出」
        // 三段分离——与手敲命令时命令紧贴输出的形态不一致。
        self.with_session(&request.session_id, |session| {
            session
                .pending_echo
                .lock()
                .expect("命令回显表锁损坏")
                .insert(markers.start.clone(), command.trim_end().to_string());
            Ok(())
        })?;
        self.with_session(&request.session_id, |session| {
            session
                .writer
                .write_all(prepared.input.as_bytes())
                .context("写入终端命令失败")?;
            session.writer.flush().context("刷新终端命令失败")
        })?;

        // 单次等待反馈预算（issue #697）：到点不中断命令，登记续等并
        // 返回 still_running——打断命令由 Agent 显式发送 `\x03`。
        let budget_secs = request
            .timeout
            .unwrap_or(EXEC_DEFAULT_WAIT_SECS)
            .clamp(1, EXEC_MAX_WAIT_SECS);
        let budget_deadline = Instant::now() + Duration::from_secs(budget_secs);
        let command_started_at = Instant::now();
        let mut exit_code_seen_at = None;
        // 静默挂起检测基线：命令写入后输出停止增长的时刻。
        let mut last_raw_total = start_offset;
        let mut last_output_at = Instant::now();
        // 夭折探测的节流状态。
        let mut aborted_probes: u32 = 0;
        let mut aborted_probe_at: Option<Instant> = None;
        loop {
            // 会话状态先行：已被回收（手动关闭或选终端回收先一步出表）时
            // 给出明确错误，不再读输出撞「会话不存在」内部错误。
            let end_state = match self.session_end_state(&request.session_id) {
                SessionEndState::Reaped => bail!(
                    "终端会话已被关闭或回收，命令结果不可恢复：{}",
                    request.session_id
                ),
                state => state,
            };
            let raw = self.raw_output_since(&request.session_id, start_offset)?;
            let parsed = parse_command_output(&raw, &markers);
            // 输出仍在增长说明命令在正常推进，刷新静默基线。
            let raw_total = start_offset.saturating_add(raw.len() as u64);
            if raw_total != last_raw_total {
                last_raw_total = raw_total;
                last_output_at = Instant::now();
            }
            // 完成判断优先于死亡收尾：结束标记已闭合时以命令结果为准，
            // shell 随后的退出只补「终端已退出」标记并回收，不覆盖命令
            // 退出码（命令带 exit 的组合、shell 自身异常退出等场景）。
            if parsed.completed {
                // end marker 已闭合后让登录 shell 完成提示符绘制，避免紧随其后的
                // terminal_send 与 wrapper 收尾交错，造成输入多字或终端状态异常。
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(self.finish_parsed(&request.session_id, parsed, end_state));
            }
            if parsed.exit_code.is_some() {
                let seen_at = exit_code_seen_at.get_or_insert_with(Instant::now);
                // 少数 TTY 程序会污染 end marker。退出码已出现但 500ms 内仍
                // 未见 end 时按已完成兜底，不能固定等到整个命令超时。
                if seen_at.elapsed() >= Duration::from_millis(500) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    return Ok(self.finish_parsed(&request.session_id, parsed, end_state));
                }
            }
            // 命令使 shell 本身退出（如 exit/登出）：end marker 不会出现，
            // 以已解析退出码（无则 shell 退出码）收尾，已收集输出照常返回。
            if matches!(end_state, SessionEndState::Ended(_)) {
                return self.ended_session_response(&request.session_id, start_offset, &markers);
            }
            // 交互程序（vi/ssh/REPL/sudo 密码等）不在 exec 内判定（issue
            // #697 终稿：sidecar 不做理解性判定）：等待窗口耗尽一律按
            // still_running 交还，Agent 从返回输出自行识别状况并决定
            // 续等收割、status 查询还是 \x03 中断。
            // 包装脚本夭折兜底：start marker 已出现，但 cwd/rc/end 边界
            // 永远不会再来了。
            //
            // 注意这里**不是** issue #571 的主修复路径：用户命令已隔离进
            // 内层脚本，`echo ===`、语法错误、命令不存在等都只会终止内层
            // `source`，wrapper 照常吐出 end marker 走正常收尾。实测只有
            // 「shell 被顶替但新 shell 仍存活」（内层 `exec sh -i`）这类
            // 才会落到这里；shell 随命令一起死亡的情况由上面的 Ended 分支
            // 接管。此外 marker 被前台程序吞掉、shell 插件干扰等无法穷举
            // 的意外也归本分支兜底。
            //
            // 本分支刻意不加 `deadline.is_none()` 前置条件（对比上面的
            // 交互程序分支）：带显式 timeout 的长命令若丢了 marker，没有
            // 本兜底就要轮询满整个超时，终端还会被标记 Unresponsive 退出
            // 复用池——正是 #571 让用户难受的那种表现。
            //
            // 判据不能只看静默时长——安静跑几分钟的编译同样静默。真正的
            // 区别是「shell 是否已回到提示符」：向 shell 写一条就绪探针，
            // 前台还有命令在跑时它只会躺在输入缓冲里不被执行；夭折后
            // shell 已回到提示符，探针立刻被执行并回显 marker。
            //
            // 探针只在前台已是登录 shell 时才发（issue #581）：前台仍被命令
            // 占据时 shell 必然没回到提示符，探针只会落进该命令的 stdin——
            // git/ssh 这类读 stdin 的程序会把它当输入吃掉，静默的长命令则
            // 在探测窗口内恰好结束时，shell 随即执行探针，被误判成夭折并
            // 丢掉尾部输出。取不到前台信息（Windows）时保持原有探测。
            if last_output_at.elapsed() >= Duration::from_secs(SILENT_ABORTED_WRAPPER_SECS)
                && parsed.exit_code.is_none()
                && aborted_probes < MAX_ABORTED_WRAPPER_PROBES
                && aborted_probe_at.is_none_or(|at: Instant| {
                    at.elapsed() >= Duration::from_secs(ABORTED_WRAPPER_PROBE_INTERVAL_SECS)
                })
                && !matches!(
                    self.foreground(&request.session_id),
                    Some(Foreground::Program { .. })
                )
            {
                aborted_probes += 1;
                aborted_probe_at = Some(Instant::now());
                if self.wrapper_aborted(&request.session_id).await? {
                    // 探针被执行只说明 shell 此刻在提示符。命令可能恰好在
                    // 探测窗口内正常结束——它的 end marker 先于探针输出，
                    // 必须以探针之后的完整输出重新解析，已完成就按正常结果
                    // 收尾，不得误报夭折、丢掉尾部输出。
                    let raw = self.raw_output_since(&request.session_id, start_offset)?;
                    let reparsed = parse_command_output(&raw, &markers);
                    if reparsed.completed || reparsed.exit_code.is_some() {
                        let end_state = self.session_end_state(&request.session_id);
                        return Ok(self.finish_parsed(&request.session_id, reparsed, end_state));
                    }
                    tracing::warn!(
                        session_id = request.session_id,
                        "命令包装脚本未正常结束，shell 已回到提示符，按异常结束收尾"
                    );
                    let mut response = reparsed.into_response();
                    response.exit_code = -1;
                    response.stderr =
                        "命令包装脚本未正常结束（命令可能存在语法/解析错误），已返回其结束前的输出"
                            .to_string();
                    return Ok(response);
                }
            }
            // 单次等待预算耗尽且命令未完成（issue #697 续等协议）：不中断
            // 命令，登记为可续等的包装命令并交还控制权——Agent 用
            // terminal(action=send, input="") 续等收割，下次可显式设置更长
            // 的 timeout；需要打断则由 Agent 主动发送 \x03。
            if Instant::now() >= budget_deadline {
                tracing::info!(
                    session_id = request.session_id,
                    budget_secs,
                    "命令超过单次等待预算仍未完成，登记续等并交还控制权"
                );
                return Ok(self.hand_over_running_command(
                    &request.session_id,
                    command,
                    command_started_at,
                    start_offset,
                    &markers,
                    budget_secs,
                ));
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// 探测「包装脚本是否已夭折」：向 shell 写一条就绪探针，在预算内
    /// 看到它被**执行**（而非仅被 tty 回显）即说明 shell 已回到提示符，
    /// 当前命令的边界标记不会再出现了。
    ///
    /// 探针写法保证回显不含完整 marker（见 [`super::shell::shell_ready_probe`]），
    /// 因此前台仍有命令在跑时不会误判：那种情况下探针只会躺在输入
    /// 缓冲里，等前台结束后才被执行——而那时命令的 end marker 早已
    /// 先一步输出，主循环会以正常完成收尾。
    ///
    /// 之所以用探针而非 `foreground_is_interactive` 取反：后者不是充分
    /// 条件，命令在两条前台进程之间的空档会瞬时成立，足以造成误判。
    async fn wrapper_aborted(&self, session_id: &str) -> Result<bool> {
        let marker = format!("__TIANGONG_ABORTCHK_{}__", scru128::new());
        let probe_offset = self.with_session(session_id, |session| Ok(session.raw_bytes_total))?;
        self.with_session(session_id, |session| {
            session
                .writer
                .write_all(shell_ready_probe(session.shell, &marker).as_bytes())
                .context("发送夭折探针失败")?;
            session.writer.flush().context("刷新夭折探针失败")
        })?;

        let deadline = Instant::now() + Duration::from_millis(ABORTED_WRAPPER_PROBE_TIMEOUT_MS);
        loop {
            let raw = self.raw_output_since(session_id, probe_offset)?;
            if shell_ready_probe_completed(&String::from_utf8_lossy(&raw), &marker) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// 命令正常完成时的收尾：shell 若已随之退出（Ended），补「终端已
    /// 退出」标记并回收会话，命令退出码保持解析结果不被覆盖。
    fn finish_parsed(
        &self,
        session_id: &str,
        parsed: ParsedCommandOutput,
        end_state: SessionEndState,
    ) -> ExecResponse {
        let mut response = parsed.into_response();
        if matches!(end_state, SessionEndState::Ended(_)) {
            response.session_ended = true;
            self.reap_ended_session(session_id);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::types::{
        AcquireRequest, SendRequest, SessionIdRequest, SpawnRequest, default_cols, default_rows,
    };

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_exec_returns_output_and_keeps_shell_alive() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        std::fs::write(cwd.path().join("terminal-result.txt"), "ok").expect("创建测试文件失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        let first = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "ls".to_string(),
                timeout: Some(10),
                cwd: None,
            })
            .await
            .expect("第一次命令执行失败");
        let raw = service
            .with_session(&spawned.session_id, |session| {
                Ok(String::from_utf8_lossy(&session.raw_history).to_string())
            })
            .unwrap_or_default();
        assert_eq!(first.exit_code, 0, "第一次命令结果: {first:?}\nraw={raw:?}");
        assert_eq!(first.stdout, "terminal-result.txt");
        assert_eq!(
            first.cwd_after,
            cwd.path()
                .canonicalize()
                .expect("规范化测试目录失败")
                .to_string_lossy()
        );

        let second = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "false".to_string(),
                timeout: Some(10),
                cwd: None,
            })
            .await
            .expect("第二次命令执行失败");
        assert_eq!(second.exit_code, 1);

        // v7 续等协议（issue #697）：等待窗口（timeout=1）耗尽命令未完成
        // → 不中断、登记续等、返回 still_running；用空 input 续等收割
        // 完整输出与退出码，之后终端仍可继续执行。
        let handed_over = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "sleep 2 && echo ran".to_string(),
                timeout: Some(1),
                cwd: None,
            })
            .await
            .expect("交还命令执行失败");
        assert!(handed_over.still_running, "窗口耗尽应交还: {handed_over:?}");

        let harvested = service
            .send_to_session(SendRequest {
                scope_id: "terminal-sidecar-test".to_string(),
                session_id: spawned.session_id.clone(),
                input: String::new(),
                wait: Some(10),
                enter: false,
            })
            .await
            .expect("续等收割失败");
        assert_eq!(harvested.status, Some("completed"), "续等应收割完成");
        assert_eq!(harvested.exit_code, 0, "退出码经 marker 精确取得");
        assert!(harvested.stdout.contains("ran"), "收割输出: {harvested:?}");

        let after_timeout = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "printf 'still-alive\\n'".to_string(),
                timeout: Some(10),
                cwd: None,
            })
            .await
            .expect("收割后的命令执行失败");
        assert_eq!(after_timeout.exit_code, 0);
        assert_eq!(after_timeout.stdout, "still-alive");
        assert!(
            service
                .with_session(&spawned.session_id, |_| Ok(()))
                .is_ok()
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// issue #571 的验证标准：`echo A; echo ===; echo B` 必须秒级返回、
    /// 三行输出齐全、退出码 0，终端保持可复用。
    /// 修复前 zsh 的 equals 展开会在解析期终止整份包装脚本，end marker
    /// 永不出现，本用例会一直轮询到超时。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zsh_裸等号命令不再挂起并返回完整输出() {
        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh", "/usr/local/bin/zsh"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
        else {
            eprintln!("跳过：本机没有 zsh");
            return;
        };
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: zsh.to_string(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建 zsh 测试终端失败");

        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "echo A; echo ===; echo B".to_string(),
                timeout: Some(15),
                cwd: None,
            })
            .await
            .expect("裸等号命令执行失败");
        assert!(!result.still_running, "裸等号命令应正常完成: {result:?}");
        assert_eq!(result.exit_code, 0, "裸等号命令应成功: {result:?}");
        let lines: Vec<&str> = result.stdout.lines().map(str::trim).collect();
        assert_eq!(lines, vec!["A", "===", "B"], "输出不完整: {result:?}");

        // 终端仍可复用：下一条命令照常执行。
        let next = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "echo still-usable".to_string(),
                timeout: Some(15),
                cwd: None,
            })
            .await
            .expect("后续命令执行失败");
        assert_eq!(next.exit_code, 0, "终端应保持可复用: {next:?}");
        assert!(next.stdout.contains("still-usable"), "{next:?}");

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// P1 兜底：用户命令自身有解析期错误（未闭合引号）时，包装脚本
    /// 仍须正常收尾——返回非零退出码与 shell 的原始报错，而不是挂到超时。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 用户命令语法错误仍按非零退出码正常收尾() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "echo before-error\necho \"unclosed\necho never".to_string(),
                timeout: Some(15),
                cwd: None,
            })
            .await
            .expect("语法错误命令执行失败");
        assert!(!result.still_running, "语法错误应正常收尾: {result:?}");
        assert_ne!(result.exit_code, 0, "语法错误应返回非零退出码: {result:?}");
        assert!(
            result.stdout.contains("before-error"),
            "错误前已执行的输出应保留: {result:?}"
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// issue #581 复现：命令静默超过夭折判定窗口（3 秒），又恰好在探测窗口
    /// 内正常结束。旧实现中 shell 结束命令后立即执行探针，被误判为「包装
    /// 脚本夭折」，返回 -1 并丢掉尾部输出（实测 `echo first; sleep 3.4;
    /// echo LATE` 稳定复现）。现在要么不发探针（前台被占据），要么命中后
    /// 重新解析，都必须得到正常退出码与完整输出。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 探测窗口内正常结束的命令不被误判夭折() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        for delay in ["3.2", "3.5"] {
            let result = service
                .exec_in_session(ExecRequest {
                    session_id: spawned.session_id.clone(),
                    cmd: format!("echo first; sleep {delay}; echo late-output"),
                    timeout: Some(30),
                    cwd: None,
                })
                .await
                .expect("命令执行失败");
            assert_eq!(
                result.exit_code, 0,
                "sleep {delay} 不得误判夭折: {result:?}"
            );
            assert!(
                result.stdout.contains("first") && result.stdout.contains("late-output"),
                "sleep {delay} 应返回完整输出: {result:?}"
            );
            assert!(result.stderr.is_empty(), "不得附带夭折说明: {result:?}");
        }

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// 静默但仍在运行的长命令绝不能被夭折兜底误判：探针躺在输入缓冲里
    /// 不被执行，`wrapper_aborted` 必须返回 false，命令照常跑到结束。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 静默长命令不被夭折兜底误判() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        // 静默 6 秒（超过 SILENT_ABORTED_WRAPPER_SECS，会触发多轮探测）
        // 后才输出，必须拿到正常退出码与完整输出。
        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "sleep 6; echo slow-done".to_string(),
                timeout: Some(30),
                cwd: None,
            })
            .await
            .expect("静默长命令执行失败");
        assert!(
            !result.still_running,
            "窗口内完成的静默命令不交还: {result:?}"
        );
        assert_eq!(result.exit_code, 0, "静默长命令应正常完成: {result:?}");
        assert!(
            result.stdout.contains("slow-done"),
            "应拿到完整输出: {result:?}"
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// P1 兜底必须真能触发：构造一个 end marker 永不出现、但 shell 已回到
    /// 提示符的场景——内层脚本用 `exec` 顶掉当前 shell 进程执行别的程序，
    /// wrapper 余下的 marker 输出随之全部丢失。
    /// 没有兜底时本用例只能等到 timeout；有兜底时秒级返回明确语义。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 包装脚本夭折时按异常结束收尾而非超时() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        let started = Instant::now();
        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                // 子 shell 里 exec 一个新登录 shell：当前 shell 被顶替，
                // wrapper 的 cwd/rc/end marker 全部不会再输出。
                cmd: "echo before-abort; exec sh -i".to_string(),
                // 不设 timeout：没有兜底就会一路轮询到工具层超时。
                timeout: None,
                cwd: None,
            })
            .await
            .expect("夭折命令执行失败");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "兜底应在静默窗口后很快收尾，实际耗时 {:?}",
            started.elapsed()
        );
        assert!(
            result.stderr.contains("包装脚本未正常结束"),
            "应按夭折给出可识别语义: {result:?}"
        );
        assert_eq!(result.exit_code, -1, "夭折应返回 -1: {result:?}");
        assert!(
            !result.still_running,
            "夭折是确定性收尾，不是交还等待: {result:?}"
        );
        assert!(
            result.stdout.contains("before-abort"),
            "夭折前的输出应保留: {result:?}"
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_exiting_shell_returns_code_output_and_reaps_session() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        // 命令先输出内容再 exit 7：退出码、此前输出与会话结束标记都应保留。
        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "echo before-exit; exit 7".to_string(),
                timeout: Some(10),
                cwd: None,
            })
            .await
            .expect("结束 shell 的命令执行失败");
        assert_eq!(result.exit_code, 7, "应以 shell 退出码收尾: {result:?}");
        assert!(
            result.stdout.contains("before-exit"),
            "应保留命令输出: {result:?}"
        );
        assert!(result.session_ended, "应标记终端已随命令退出: {result:?}");
        assert!(
            service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .get(&spawned.session_id)
                .is_none(),
            "死亡会话应已出表"
        );

        // 同一会话紧接着执行：选终端判定无可用（旧死终端已清、不占用
        // all_busy 名额），新建终端后正常返回。
        let acquired = service.acquire_session(AcquireRequest {
            scope_id: "terminal-sidecar-test".to_string(),
        });
        assert_eq!(
            acquired.reason, "no_available",
            "死终端不应计入可用或占用名额: {acquired:?}"
        );
        let respawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("重建测试终端失败");
        let second = service
            .exec_in_session(ExecRequest {
                session_id: respawned.session_id.clone(),
                cmd: "echo recovered".to_string(),
                timeout: Some(10),
                cwd: None,
            })
            .await
            .expect("死终端后的命令执行失败");
        assert_eq!(second.exit_code, 0);
        assert_eq!(second.stdout, "recovered");

        service
            .kill_session(SessionIdRequest {
                session_id: respawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// 判据必须能区分「交互程序在等按键」与「长命令在安静地跑」。
    ///
    /// 这是本修复的核心安全性质：长时间的编译、测试不能被误判打断。
    /// 用 `sleep` 代表安静的长命令——它占据前台但不碰 tty 模式，
    /// 与 `cargo build` / `pytest` 的 tty 行为一致。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 交互程序与静默长命令的前台判定必须可区分() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(workspace.clone()),
                scope_id: Some("session-hang".to_string()),
                reserve: false,
                cols: default_cols(),
                rows: default_rows(),
            })
            .expect("创建终端失败");
        let terminal = spawned.session_id;

        let wait_until = |expected: bool| {
            let service = &service;
            let terminal = terminal.clone();
            async move {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let interactive = matches!(
                        service.foreground(&terminal),
                        Some(Foreground::Program { interactive: true })
                    );
                    if interactive == expected {
                        return true;
                    }
                    if Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        let write = |data: &'static [u8]| {
            service
                .with_session(&terminal, |session| {
                    session.writer.write_all(data)?;
                    session.writer.flush()?;
                    Ok(())
                })
                .expect("写入终端失败");
        };

        // 空闲 shell：没有命令在跑。
        assert!(wait_until(false).await, "空闲终端不应判定为交互等待");

        // 安静的长命令：占据前台，但不改 tty 模式 —— 不得判为交互。
        // 这一条挂掉就意味着长时间编译/测试会被错误打断。
        write(b"sleep 30\r");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !matches!(
                service.foreground(&terminal),
                Some(Foreground::Program { interactive: true })
            ),
            "安静的长命令（编译/测试同类）不得被判定为交互等待"
        );

        // 交互程序：切换 tty 为非规范模式逐键读取 —— 应判为交互。
        write(b"\x03");
        assert!(wait_until(false).await, "中断后应回到 shell");
        write(b"python3 -q\r");
        assert!(wait_until(true).await, "交互 REPL 应被判定为正在等待输入");

        write(b"exit()\r");
        let _ = service.kill_session(SessionIdRequest {
            session_id: terminal,
        });
    }
}
