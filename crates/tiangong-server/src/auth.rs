use silent::prelude::*;

/// 从请求中提取 Bearer Token 并与预配置 token 比对。
/// 如果 expected_token 为 None，则跳过认证（无 token 配置时视为开放）。
/// 返回 Ok(()) 表示通过，Err 表示 401。
pub fn check_auth(
    req: &Request,
    expected_token: Option<&str>,
) -> std::result::Result<(), SilentError> {
    let Some(expected) = expected_token else {
        // 未配置 token，免认证
        return Ok(());
    };

    check_auth_headers(req.headers(), expected)
}

fn check_auth_headers(
    headers: &silent::header::HeaderMap<silent::header::HeaderValue>,
    expected: &str,
) -> std::result::Result<(), SilentError> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());

    match auth_header {
        Some(value) if value.starts_with("Bearer ") => {
            let provided = &value[7..];
            if provided == expected {
                Ok(())
            } else {
                Err(SilentError::business_error(
                    StatusCode::UNAUTHORIZED,
                    "认证失败：Token 不匹配".to_string(),
                ))
            }
        }
        _ => Err(SilentError::business_error(
            StatusCode::UNAUTHORIZED,
            "认证失败：缺少 Bearer Token".to_string(),
        )),
    }
}
