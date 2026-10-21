//! 截图产物规划（平台无关纯函数，macOS / Windows 共用）。
//!
//! 产物按截取范围的逻辑尺寸输出（1 图片像素 = 1 屏幕坐标单位：macOS 为
//! points，Windows 为物理像素）；长边超过上限时按 1/2、1/4… 整数倍缩小，
//! 不裁剪画面，响应的 `scale` 即缩小因子。

/// 截图 JPEG 质量（0-100）。
pub(crate) const SCREENSHOT_JPEG_QUALITY: u32 = 75;

/// 截图输出规划：目标像素尺寸与图片像素→屏幕坐标倍率。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenshotPlan {
    pub width: u32,
    pub height: u32,
    pub factor: u32,
}

/// 由原始像素尺寸、逻辑尺寸与长边上限计算输出尺寸。
///
/// 逻辑尺寸缺失（0）时退回原始像素（视作 1x 屏）；目标取逻辑尺寸
/// 除以 2 的幂次因子，保证 `屏幕坐标 = 起点 + 图片坐标 × factor`。
pub(crate) fn plan_screenshot_output(
    raw: (u32, u32),
    logical: (f64, f64),
    max_dimension: u32,
) -> ScreenshotPlan {
    let (lw, lh) = if logical.0 >= 1.0 && logical.1 >= 1.0 {
        logical
    } else {
        (f64::from(raw.0), f64::from(raw.1))
    };
    let factor = tiangong_plugin_computer_use_protocol::ops::screenshot_downscale_factor(
        lw,
        lh,
        max_dimension,
    );
    let div = f64::from(factor);
    ScreenshotPlan {
        width: ((lw / div).round() as u32).max(1),
        height: ((lh / div).round() as u32).max(1),
        factor,
    }
}

/// 截图落点：`<存储根>/media/screenshots`。存储根由宿主经
/// `TIANGONG_STORAGE_ROOT` 注入；缺失时退回系统临时目录（截图仍可用，
/// 但不参与媒体目录的统一管理）。
pub(crate) fn screenshot_output_dir() -> std::path::PathBuf {
    let root = std::env::var(tiangong_plugin_sidecar::env::STORAGE_ROOT_ENV)
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    root.join("media").join("screenshots")
}

#[cfg(test)]
mod tests {
    use super::{ScreenshotPlan, plan_screenshot_output};

    #[test]
    fn retina_region_within_limit_normalizes_to_logical_size() {
        // 800×600 points 区域在 2x 屏截出 1600×1200 物理像素 → 输出 800×600、scale 1。
        let plan = plan_screenshot_output((1600, 1200), (800.0, 600.0), 1568);
        assert_eq!(
            plan,
            ScreenshotPlan {
                width: 800,
                height: 600,
                factor: 1
            }
        );
    }

    #[test]
    fn full_screen_over_limit_halves_instead_of_cropping() {
        // 2560×1440 主屏（5120×2880 物理）→ 1/2：1280×720，scale 2。
        let plan = plan_screenshot_output((5120, 2880), (2560.0, 1440.0), 1568);
        assert_eq!(
            plan,
            ScreenshotPlan {
                width: 1280,
                height: 720,
                factor: 2
            }
        );
        // 自定义更小上限 → 1/4。
        let plan = plan_screenshot_output((5120, 2880), (2560.0, 1440.0), 800);
        assert_eq!(plan.factor, 4);
        assert_eq!((plan.width, plan.height), (640, 360));
    }

    #[test]
    fn missing_logical_size_falls_back_to_raw_pixels() {
        let plan = plan_screenshot_output((1000, 500), (0.0, 0.0), 1568);
        assert_eq!((plan.width, plan.height, plan.factor), (1000, 500, 1));
    }
}
