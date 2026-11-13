//! 风险分级（设计稿 §14.1）。
//!
//! | 级别 | 示例 | 默认策略 |
//! | --- | --- | --- |
//! | 低 | 读取文件、分析日志、查看 Git 状态、运行本地测试、生成报告 | 可按设置自动安排 |
//! | 中 | 修改代码、创建分支、安装依赖、启动开发服务、切换工作会话模型 | 默认说明计划并确认，或依赖用户预授权 |
//! | 高 | 删除文件、生产部署、数据库迁移、强制推送、外部消息、付费、权限变化 | 最后一步必须明确确认 |

use serde::{Deserialize, Serialize};

/// 操作风险级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low,
    Medium,
    High,
}

impl RiskLevel {
    /// 该级别是否允许不经用户确认自动安排（§14.1：仅低风险）。
    pub fn auto_dispatch_allowed(self) -> bool {
        matches!(self, RiskLevel::Low)
    }

    /// 该级别是否必须在最后一步获得用户明确确认（§14.1：高风险）。
    pub fn requires_final_confirmation(self) -> bool {
        matches!(self, RiskLevel::High)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_matches_design() {
        assert!(RiskLevel::Low.auto_dispatch_allowed());
        assert!(!RiskLevel::Low.requires_final_confirmation());

        assert!(!RiskLevel::Medium.auto_dispatch_allowed());
        assert!(!RiskLevel::Medium.requires_final_confirmation());

        assert!(!RiskLevel::High.auto_dispatch_allowed());
        assert!(RiskLevel::High.requires_final_confirmation());
    }

    #[test]
    fn serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&RiskLevel::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::from_str::<RiskLevel>("\"high\"").unwrap(),
            RiskLevel::High
        );
    }
}
