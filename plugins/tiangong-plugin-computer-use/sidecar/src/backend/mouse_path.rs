//! 平台无关的鼠标曲线/多点路径规划。
//!
//! 轨迹在触达 CGEvent/SendInput 前一次性规划完成：按弧长均匀重采样，
//! 避免贝塞尔参数速度不均造成绘图抖动；总事件数设硬上限，确保长路径
//! 不会耗尽 60 秒工具预算。macOS 与 Windows 共用本模块。

use std::time::Duration;

use tiangong_plugin_computer_use_protocol::ops::{MAX_MOUSE_PATH_POINTS, MouseGesture, MousePoint};

/// 轨迹相邻事件的目标间距与间隔。约 400px/s，兼顾平滑度与绘图耗时。
pub const MOTION_STEP_PX: f64 = 4.0;
pub const MOTION_STEP_INTERVAL: Duration = Duration::from_millis(10);
/// 单次轨迹最多投递的事件数（含终点），最长阻塞约 20.5 秒。
pub const MAX_MOTION_STEPS: usize = 2048;

const CURVE_SAMPLES: usize = 512;

/// 按手势规划曲线/多点轨迹；普通手势返回 `None`。
pub fn plan_for_gesture(
    gesture: MouseGesture,
    start_x: f64,
    start_y: f64,
    to: Option<(f64, f64)>,
    control: Option<(f64, f64)>,
    points: Option<&[MousePoint]>,
) -> Option<Result<Vec<MousePoint>, String>> {
    match gesture {
        MouseGesture::MoveCurve | MouseGesture::DragCurve => {
            let Some((control_x, control_y)) = control else {
                return Some(Err("曲线手势缺少 control_x/control_y".to_string()));
            };
            let Some((end_x, end_y)) = to else {
                return Some(Err("曲线手势缺少 to_x/to_y".to_string()));
            };
            Some(plan_curve(
                MousePoint {
                    x: start_x,
                    y: start_y,
                },
                MousePoint {
                    x: control_x,
                    y: control_y,
                },
                MousePoint { x: end_x, y: end_y },
            ))
        }
        MouseGesture::MovePath | MouseGesture::DragPath => {
            let Some(points) = points else {
                return Some(Err("多点手势缺少 points".to_string()));
            };
            Some(plan_path(points))
        }
        _ => None,
    }
}

/// 规划二次贝塞尔曲线：起点 → 控制点 → 终点，按弧长均匀采样。
pub fn plan_curve(
    start: MousePoint,
    control: MousePoint,
    end: MousePoint,
) -> Result<Vec<MousePoint>, String> {
    for (name, point) in [("起点", start), ("控制点", control), ("终点", end)] {
        ensure_finite_point(point, name)?;
    }
    let samples = (0..=CURVE_SAMPLES)
        .map(|index| {
            let t = index as f64 / CURVE_SAMPLES as f64;
            MousePoint {
                x: quadratic_at(start.x, control.x, end.x, t),
                y: quadratic_at(start.y, control.y, end.y, t),
            }
        })
        .collect::<Vec<_>>();
    resample_dense_path(&samples)
}

/// 规划多点折线，保留每个输入路点（含拐角），再按弧长均匀细分。
pub fn plan_path(points: &[MousePoint]) -> Result<Vec<MousePoint>, String> {
    if !(2..=MAX_MOUSE_PATH_POINTS).contains(&points.len()) {
        return Err(format!("多点路径必须包含 2～{MAX_MOUSE_PATH_POINTS} 个点"));
    }
    for (index, point) in points.iter().enumerate() {
        ensure_finite_point(*point, &format!("points[{index}]"))?;
    }
    resample_polyline(points)
}

fn ensure_finite_point(point: MousePoint, name: &str) -> Result<(), String> {
    if point.x.is_finite() && point.y.is_finite() {
        Ok(())
    } else {
        Err(format!("鼠标路径{name}必须为有限数值"))
    }
}

fn quadratic_at(start: f64, control: f64, end: f64, t: f64) -> f64 {
    (1.0 - t) * (1.0 - t) * start + 2.0 * (1.0 - t) * t * control + t * t * end
}

/// 对已密集采样的曲线按累计弧长等距重采样。
fn resample_dense_path(samples: &[MousePoint]) -> Result<Vec<MousePoint>, String> {
    let cumulative = cumulative_lengths(samples)?;
    let total = *cumulative.last().expect("曲线采样不能为空");
    if total <= f64::EPSILON {
        return Ok(vec![samples[0]]);
    }
    let steps = ((total / MOTION_STEP_PX).ceil() as usize).clamp(1, MAX_MOTION_STEPS);
    let mut result = Vec::with_capacity(steps + 1);
    result.push(samples[0]);
    let mut segment = 0;
    for step in 1..=steps {
        let target = total * step as f64 / steps as f64;
        while segment + 1 < cumulative.len() - 1 && cumulative[segment + 1] < target {
            segment += 1;
        }
        let segment_length = cumulative[segment + 1] - cumulative[segment];
        let local = if segment_length > f64::EPSILON {
            ((target - cumulative[segment]) / segment_length).clamp(0.0, 1.0)
        } else {
            0.0
        };
        result.push(lerp(samples[segment], samples[segment + 1], local));
    }
    Ok(result)
}

/// 折线重采样：每个原始路点都保留，步数按段长比例分配，避免拐角被切掉。
fn resample_polyline(points: &[MousePoint]) -> Result<Vec<MousePoint>, String> {
    let cumulative = cumulative_lengths(points)?;
    let total = *cumulative.last().expect("路径不能为空");
    if total <= f64::EPSILON {
        return Ok(vec![points[0]]);
    }
    let minimum_steps = points.len() - 1;
    let steps = ((total / MOTION_STEP_PX).ceil() as usize).clamp(minimum_steps, MAX_MOTION_STEPS);
    let mut result = Vec::with_capacity(steps + 1);
    result.push(points[0]);
    let mut remaining = steps;
    for segment in 0..points.len() - 1 {
        let segments_left = points.len() - 1 - segment;
        let segment_steps = if segments_left == 1 {
            remaining
        } else {
            let segment_length = cumulative[segment + 1] - cumulative[segment];
            let proportional = (segment_length / total * steps as f64).round() as usize;
            proportional.clamp(1, remaining - (segments_left - 1))
        };
        for step in 1..=segment_steps {
            let t = step as f64 / segment_steps as f64;
            result.push(lerp(points[segment], points[segment + 1], t));
        }
        remaining -= segment_steps;
    }
    debug_assert_eq!(remaining, 0);
    Ok(result)
}

fn cumulative_lengths(points: &[MousePoint]) -> Result<Vec<f64>, String> {
    let mut cumulative = Vec::with_capacity(points.len());
    cumulative.push(0.0);
    for pair in points.windows(2) {
        let dx = pair[1].x - pair[0].x;
        let dy = pair[1].y - pair[0].y;
        let length = dx.hypot(dy);
        if !length.is_finite() {
            return Err("鼠标路径长度超出有效数值范围".to_string());
        }
        let previous = *cumulative.last().expect("累计长度不能为空");
        let next = previous + length;
        if !next.is_finite() {
            return Err("鼠标路径总长度超出有效数值范围".to_string());
        }
        cumulative.push(next);
    }
    Ok(cumulative)
}

fn lerp(start: MousePoint, end: MousePoint, t: f64) -> MousePoint {
    MousePoint {
        x: start.x + (end.x - start.x) * t,
        y: start.y + (end.y - start.y) * t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f64, y: f64) -> MousePoint {
        MousePoint { x, y }
    }

    #[test]
    fn 曲线保留端点且经过贝塞尔中点() {
        let (start, control, end) = (point(0.0, 0.0), point(50.0, 100.0), point(100.0, 0.0));
        let curve = plan_curve(start, control, end).unwrap();
        assert_eq!(curve.first(), Some(&start));
        assert_eq!(curve.last(), Some(&end));
        assert_eq!(quadratic_at(start.x, control.x, end.x, 0.5), 50.0);
        assert_eq!(quadratic_at(start.y, control.y, end.y, 0.5), 50.0);
        // 弧长均匀采样不保证 t=0.5 恰好落在采样点上，但应经过中点附近。
        assert!(curve.iter().any(|p| (p.x - 50.0).hypot(p.y - 50.0) < 2.1));
        assert!(curve.len() > 3);
    }

    #[test]
    fn 曲线按弧长均匀采样() {
        let curve = plan_curve(point(0.0, 0.0), point(0.0, 200.0), point(100.0, 0.0)).unwrap();
        let distances = curve
            .windows(2)
            .map(|pair| (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y))
            .collect::<Vec<_>>();
        let min = distances.iter().copied().fold(f64::INFINITY, f64::min);
        let max = distances.iter().copied().fold(0.0, f64::max);
        assert!((max - min) / max < 0.01, "步长应近似相等：{min}..{max}");
    }

    #[test]
    fn 多点路径保留起点终点和每个拐角() {
        let points = [point(0.0, 0.0), point(100.0, 0.0), point(100.0, 100.0)];
        let path = plan_path(&points).unwrap();
        assert_eq!(path.first(), Some(&points[0]));
        assert_eq!(path.last(), Some(&points[2]));
        assert!(path.contains(&points[1]));
    }

    #[test]
    fn 长路径事件数受限且终点精确() {
        let points = [point(0.0, 0.0), point(1_000_000.0, 1_000_000.0)];
        let path = plan_path(&points).unwrap();
        assert_eq!(path.len(), MAX_MOTION_STEPS + 1);
        assert_eq!(path.last(), Some(&points[1]));
    }

    #[test]
    fn 重复路径点允许作为原地按下抬起() {
        let points = [point(10.0, 20.0), point(10.0, 20.0)];
        assert_eq!(plan_path(&points).unwrap(), vec![points[0]]);
    }

    #[test]
    fn 拒绝非有限坐标和非法点数() {
        assert!(plan_curve(point(0.0, 0.0), point(f64::NAN, 1.0), point(2.0, 2.0)).is_err());
        assert!(plan_path(&[point(0.0, 0.0)]).is_err());
        assert!(plan_path(&[point(0.0, 0.0), point(f64::INFINITY, 1.0)]).is_err());
    }
}
