//! 目标与报告工具（R5 从 lib.rs 拆出）：create_goal / update_goal_status /
//! create_report / list_reports（§13.2 / §13.5）。

use tiangong_core::tools::result::ToolResult;

use crate::CentralPlugin;
use crate::*;

impl CentralPlugin {
    /// 用户目标登记（§13.2）：目标跨工作持续存在，get_context 注入当前目标。
    pub(crate) fn handle_create_goal(
        &self,
        title: &str,
        description: &str,
        workspace: Option<&str>,
    ) -> Option<ToolResult> {
        let title = title.trim();
        if title.is_empty() {
            return self.error_result("title 不能为空".to_string());
        }
        let mut goals = load_goals(self.sessions.storage_root());
        // 同名幂等：已存在的同名 active 目标直接返回
        if let Some(existing) = goals
            .iter()
            .find(|g| g.status == "active" && g.title == title)
        {
            return self.text_result(
                "目标已存在",
                format!(
                    "目标「{title}」已存在（{}），无需重复创建。",
                    existing.goal_id
                ),
            );
        }
        let goal = GoalRecord {
            goal_id: format!("goal-{}", scru128::new()),
            title: title.to_string(),
            description: description.trim().to_string(),
            workspace: workspace
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(str::to_string),
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            status: "active".to_string(),
        };
        let goal_id = goal.goal_id.clone();
        goals.push(goal);
        // active 上限 10：最旧的 active 归档为 achieved（不物理删除，留痕）
        let active_count = goals.iter().filter(|g| g.status == "active").count();
        if active_count > 10 {
            if let Some(position) = goals.iter().position(|g| g.status == "active") {
                goals[position].status = "archived".to_string();
            }
        }
        if let Err(error) = save_goals(self.sessions.storage_root(), &goals) {
            tracing::warn!(%error, "报告存档落盘失败（Z4：写失败显式可见）");
        }
        self.text_result(
            "目标已登记",
            format!(
                "目标「{title}」（{goal_id}）已记录；后续派发与汇报将围绕该目标组织，用户问「我的目标」时可引用。达成或放弃时用 update_goal_status 更新。"
            ),
        )
    }

    /// 目标状态更新（achieved / abandoned / archived）。
    pub(crate) fn handle_update_goal_status(
        &self,
        goal_id: &str,
        status: &str,
    ) -> Option<ToolResult> {
        let goal_id = goal_id.trim();
        if goal_id.is_empty() {
            return self.error_result("goal_id 不能为空".to_string());
        }
        if !["achieved", "abandoned", "archived", "active"].contains(&status) {
            return self.error_result(format!(
                "status 仅支持 active / achieved / abandoned / archived，收到 {status}"
            ));
        }
        let mut goals = load_goals(self.sessions.storage_root());
        let Some(goal) = goals.iter_mut().rev().find(|g| g.goal_id == goal_id) else {
            return self.error_result(format!("未找到目标 {goal_id}"));
        };
        goal.status = status.to_string();
        let title = goal.title.clone();
        if let Err(error) = save_goals(self.sessions.storage_root(), &goals) {
            tracing::warn!(%error, "报告存档落盘失败（Z4：写失败显式可见）");
        }
        self.text_result("目标状态已更新", format!("目标「{title}」→ {status}"))
    }

    /// 结构化报告存档（§13.5）。
    pub(crate) fn handle_create_report(
        &self,
        work_id: &str,
        summary: &str,
        artifacts: &[String],
        risks: &[String],
    ) -> Option<ToolResult> {
        let work_id = work_id.trim();
        let summary = summary.trim();
        if work_id.is_empty() || summary.is_empty() {
            return self.error_result("work_id 与 summary 不能为空".to_string());
        }
        let report = ReportRecord {
            report_id: format!("report-{}", scru128::new()),
            work_id: work_id.to_string(),
            summary: summary.to_string(),
            artifacts: artifacts.to_vec(),
            risks: risks.to_vec(),
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        };
        let report_id = report.report_id.clone();
        let mut reports = load_reports(self.sessions.storage_root());
        reports.push(report);
        let overflow = reports.len().saturating_sub(50);
        if overflow > 0 {
            reports.drain(..overflow);
        }
        if let Err(error) = save_reports(self.sessions.storage_root(), &reports) {
            tracing::warn!(%error, "报告存档落盘失败（Z4：写失败显式可见）");
        }
        self.text_result(
            "报告已存档",
            format!(
                "报告 {report_id}（工作 {work_id}）已持久化；产物 {} 项、风险 {} 项。用户需要历史结论时可查询。",
                artifacts.len(),
                risks.len()
            ),
        )
    }

    /// 报告查询（全部或按 work_id 过滤）。
    pub(crate) fn handle_list_reports(&self, work_id: Option<&str>) -> Option<ToolResult> {
        let reports = load_reports(self.sessions.storage_root());
        if reports.is_empty() {
            return self.text_result("报告存档", "暂无已存档报告。".to_string());
        }
        let filtered: Vec<&ReportRecord> = match work_id.map(str::trim).filter(|w| !w.is_empty()) {
            Some(work) => reports.iter().filter(|r| r.work_id == work).collect(),
            None => reports.iter().collect(),
        };
        if filtered.is_empty() {
            return self.text_result("报告存档", format!("工作 {work_id:?} 没有已存档报告。"));
        }
        let lines: Vec<String> = filtered
            .iter()
            .rev()
            .map(|r| {
                format!(
                    "- {}（{}，工作 {}）：{}{}{}",
                    r.report_id,
                    r.created_at,
                    r.work_id,
                    r.summary,
                    if r.artifacts.is_empty() {
                        String::new()
                    } else {
                        format!("；产物：{}", r.artifacts.join("、"))
                    },
                    if r.risks.is_empty() {
                        String::new()
                    } else {
                        format!("；风险：{}", r.risks.join("、"))
                    }
                )
            })
            .collect();
        self.text_result(
            "报告存档",
            format!("共 {} 份：\n{}", filtered.len(), lines.join("\n")),
        )
    }
}
