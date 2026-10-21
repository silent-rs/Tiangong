//! 宿主向 sidecar 进程注入的环境变量约定。
//!
//! 宿主（`tiangong-plugin-runtime`）按这些名字设置子进程环境，sidecar 按同名
//! 读取；双方共用本模块，避免各自维护字符串常量。

/// 插件 ID。
pub const PLUGIN_ID_ENV: &str = "TIANGONG_PLUGIN_ID";
/// 插件版本。
pub const PLUGIN_VERSION_ENV: &str = "TIANGONG_PLUGIN_VERSION";
/// TCP 传输的 endpoint 文件路径。
pub const PLUGIN_ENDPOINT_ENV: &str = "TIANGONG_PLUGIN_ENDPOINT";
/// 插件安装目录下的数据目录。
pub const PLUGIN_DATA_DIR_ENV: &str = "TIANGONG_PLUGIN_DATA_DIR";
/// 插件在存储根统一写域下的自有数据目录（`~/.tiangong/data/<插件 id>`）。
/// 沙箱内存储根只有该目录、`media/` 与 `tmp/` 可写；插件自有数据应落在这里。
pub const PLUGIN_STORAGE_DATA_DIR_ENV: &str = "TIANGONG_PLUGIN_STORAGE_DATA_DIR";
/// 天工存储根。
pub const STORAGE_ROOT_ENV: &str = "TIANGONG_STORAGE_ROOT";
/// 宿主透传给命令执行环境的变量（JSON 对象）。
pub const EXEC_ENV_JSON_ENV: &str = "TIANGONG_EXEC_ENV_JSON";
/// 本机 server 的 HTTP 地址（如 `http://127.0.0.1:8080`），供需要回调 host 的
/// sidecar（如 scheduler 到点投递消息）使用。
pub const SERVER_URL_ENV: &str = "TIANGONG_SERVER_URL";
/// 本机 server 的鉴权 token（可选，未配置鉴权时为空）。
pub const SERVER_TOKEN_ENV: &str = "TIANGONG_SERVER_TOKEN";
/// sidecar 实例角色：`primary` 为注册表持有的共享连接进程（常驻服务
/// 只应在该进程内建立 IM 长连接等独占外部资源）；`auxiliary` 为安装
/// 验证、按需直连等临时进程，只应完成握手与无副作用的请求。
pub const INSTANCE_ROLE_ENV: &str = "TIANGONG_PLUGIN_INSTANCE_ROLE";
/// 共享连接进程的角色值。
pub const INSTANCE_ROLE_PRIMARY: &str = "primary";
/// 临时进程的角色值。
pub const INSTANCE_ROLE_AUXILIARY: &str = "auxiliary";

/// 传输模式标记。
pub const TRANSPORT_ENV: &str = "TIANGONG_PLUGIN_TRANSPORT";
/// stdio 传输模式的取值。
pub const TRANSPORT_STDIO: &str = "stdio";
/// stdio 模式的认证 token。
pub const STDIO_TOKEN_ENV: &str = "TIANGONG_PLUGIN_STDIO_TOKEN";
/// 创建并持有 stdio 连接的宿主进程 PID。
pub const HOST_PID_ENV: &str = "TIANGONG_PLUGIN_HOST_PID";
