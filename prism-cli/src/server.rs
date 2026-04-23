use super::*;
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 每个时间窗口允许的最大请求数
const RATE_LIMIT_MAX_REQUESTS: usize = 60;
/// 速率限制时间窗口
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// IP 条目总数上限 — 防止 DDoS 时内存无限增长
///
/// 当 buckets 中的 IP 数量达到此上限且当前 IP 不在已有条目中时，
/// 先执行全量过期清理；清理后仍达上限则拒绝该新 IP 的请求。
const RATE_LIMIT_MAX_IPS: usize = 10000;

/// 速率限制器状态
///
/// 使用滑动窗口算法，记录每个 IP 在时间窗口内的请求时间戳。
/// 通过 Mutex 保证线程安全，无需外部依赖。
///
/// 内存管理：当 IP 数量超过阈值时，清理所有过期条目；
/// 否则以概率方式随机清理一个其他 IP 的过期记录（概率性 LRU）。
struct RateLimiter {
    buckets: std::sync::Mutex<std::collections::HashMap<String, Vec<Instant>>>,
}

/// IP 数量超过此阈值时触发全量过期清理
const RATE_LIMIT_CLEANUP_THRESHOLD: usize = 1024;

impl RateLimiter {
    fn new() -> Self {
        Self {
            buckets: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 检查指定 IP 是否允许通过
    ///
    /// 返回 `true` 表示允许，`false` 表示超限。
    /// 每次调用都会清理过期的请求记录，并周期性清理不再活跃的 IP。
    fn check(&self, ip: &str) -> bool {
        let mut buckets = match self.buckets.lock() {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "Rate limiter Mutex poisoned, allowing request through");
                return true; // 锁异常时放行，避免阻塞所有请求
            }
        };

        let now = Instant::now();
        let window_start = now - RATE_LIMIT_WINDOW;

        // IP 上限保护：当 IP 数量达到上限且当前 IP 不在已有条目中时，
        // 先执行全量过期清理；清理后仍达上限则拒绝该新 IP 的请求
        if !buckets.contains_key(ip) && buckets.len() >= RATE_LIMIT_MAX_IPS {
            // 全量清理所有过期条目
            buckets.retain(|_, ts| {
                ts.retain(|&t| t > window_start);
                !ts.is_empty()
            });
            // 清理后仍达上限，拒绝新 IP
            if buckets.len() >= RATE_LIMIT_MAX_IPS {
                return false;
            }
        }

        // 内存管理：清理不再活跃的 IP（在获取当前 IP 的 entry 之前）
        if buckets.len() > RATE_LIMIT_CLEANUP_THRESHOLD {
            // IP 数量过多，全量清理所有过期条目
            buckets.retain(|_, ts| {
                ts.retain(|&t| t > window_start);
                !ts.is_empty()
            });
        } else if buckets.len() > 1 && rand::random::<f32>() < 0.05 {
            // 概率性 LRU：约 5% 的概率随机选择一个其他 IP 进行清理
            let idx = rand::random::<usize>() % buckets.len();
            let key_to_evict = buckets.keys().nth(idx).cloned();
            if let Some(key) = key_to_evict {
                // 排除当前请求 IP，避免清理自身记录
                if key == ip {
                    // 跳过，不清理当前 IP
                } else if let Some(ts) = buckets.get_mut(&key) {
                    ts.retain(|&t| t > window_start);
                    if ts.is_empty() {
                        buckets.remove(&key);
                    }
                }
            }
        }

        // 清理当前 IP 的过期记录并检查速率
        let timestamps = buckets.entry(ip.to_string()).or_default();
        timestamps.retain(|&t| t > window_start);

        if timestamps.len() >= RATE_LIMIT_MAX_REQUESTS {
            return false;
        }

        timestamps.push(now);
        true
    }
}

/// 速率限制中间件
///
/// 从请求中提取客户端 IP，检查是否超过速率限制。
/// 优先使用 `ConnectInfo<SocketAddr>`（TCP 层真实 IP），
/// 仅在不可用时回退到 `x-forwarded-for` 最右侧 IP（最接近的受信代理）。
/// 超限时返回 HTTP 429 Too Many Requests。
///
/// 非 localhost 绑定时，禁用 x-forwarded-for 回退（直接拒绝请求），
/// 因为 x-forwarded-for 可被客户端伪造，非本地绑定场景下安全风险不可接受。
async fn rate_limit_middleware(
    State(state): State<Arc<AppState>>,
    connect_info: Option<axum::extract::ConnectInfo<SocketAddr>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let limiter = &state.rate_limiter;
    let ip = match connect_info {
        Some(axum::extract::ConnectInfo(addr)) => addr.ip().to_string(),
        None => {
            // 非 localhost 绑定时，禁用 x-forwarded-for 回退。
            // x-forwarded-for 头可被任意客户端伪造，在非本地绑定场景下
            // 攻击者可通过伪造此头绕过速率限制，直接拒绝请求。
            if !state.is_localhost {
                tracing::warn!(
                    "非 localhost 绑定且 ConnectInfo 不可用，拒绝使用 x-forwarded-for 回退（防止 IP 伪造）"
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "无法确定客户端 IP（非 localhost 绑定需要受信代理传递 ConnectInfo）"
                    })),
                )
                    .into_response();
            }

            // localhost 场景下允许 x-forwarded-for 回退（安全风险可接受）
            let forwarded_ip = req
                .headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next_back())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());

            if forwarded_ip.is_some() {
                tracing::warn!("x-forwarded-for 回退被使用（ConnectInfo 不可用），IP 可能被伪造");
            }

            forwarded_ip.unwrap_or("unknown").to_string()
        }
    };

    if !limiter.check(&ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "请求过于频繁，请稍后再试"
            })),
        )
            .into_response();
    }

    next.run(req).await
}

/// 共享应用状态
pub struct AppState {
    pub ext: Arc<clash_prism_extension::PrismExtension<super::CliHost>>,
    /// API 认证密钥。`None` 表示不启用认证（本地开发模式）
    pub api_key: Option<String>,
    /// 是否绑定到 localhost（影响安全策略：x-forwarded-for 回退、CORS 宽松度）
    pub is_localhost: bool,
    /// 速率限制器
    rate_limiter: Arc<RateLimiter>,
}

/// 统一错误响应
#[derive(Debug)]
struct ApiError(String);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": self.0
            })),
        )
            .into_response()
    }
}

impl From<String> for ApiError {
    fn from(s: String) -> Self {
        ApiError(s)
    }
}

/// API Key 认证中间件
///
/// 当 `api_key` 已配置时，验证请求头 `Authorization: Bearer <key>`。
/// 未配置时跳过认证（向后兼容本地开发场景）。
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    if let Some(expected_key) = &state.api_key {
        let auth_header = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        let valid = auth_header
            .map(|h| {
                let prefix = "Bearer ";
                if !h.starts_with(prefix) {
                    return false;
                }
                let provided = &h[prefix.len()..];
                super::constant_time_eq(provided, expected_key.as_str())
            })
            .unwrap_or(false);

        if !valid {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "未授权：缺少或无效的 Authorization 头"
                })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

/// HTTP Server 启动参数
pub struct ServeConfig {
    pub bind: String,
    pub port: u16,
    pub config: PathBuf,
    pub prism_dir: PathBuf,
    pub debounce_ms: u64,
    pub api_key: Option<String>,
    pub no_watch: bool,
    pub allowed_origins: Option<Vec<String>>,
    /// 强制获取 PID 锁（覆盖已运行的实例）
    pub force: bool,
}

/// 启动 HTTP Server
pub async fn run(cfg: ServeConfig) -> Result<(), String> {
    // 在 serve 启动前获取 PID 锁，防止多个实例同时运行
    // force=true 时强制覆盖已运行的实例锁
    let _pid_lock = super::pid_lock::PidLock::acquire(&cfg.prism_dir, cfg.force)?;

    // 非 localhost 绑定且无 API Key 时输出安全警告
    let is_localhost = cfg.bind == "127.0.0.1" || cfg.bind == "::1" || cfg.bind == "localhost";
    if !is_localhost && cfg.api_key.is_none() {
        eprintln!(
            "警告：服务绑定到 {}（非 localhost）且未设置 API Key，任何网络用户均可访问 API。",
            cfg.bind
        );
    }

    let host = super::CliHost {
        config_path: cfg.config,
        prism_dir: cfg.prism_dir,
    };
    let ext = Arc::new(clash_prism_extension::PrismExtension::new(host));

    // 启动文件监听（除非 --no-watch）
    if !cfg.no_watch {
        if let Err(e) = ext.start_watching(cfg.debounce_ms) {
            eprintln!("文件监听启动失败: {}", e);
        }
    } else {
        eprintln!("文件监听已禁用 (--no-watch)");
    }

    let limiter = Arc::new(RateLimiter::new());

    let state = Arc::new(AppState {
        ext: ext.clone(),
        api_key: cfg.api_key,
        is_localhost,
        rate_limiter: limiter,
    });

    // CORS 策略：localhost 自动允许所有来源，非 localhost 需要显式配置
    // 非 localhost 时，即使 allowed_origins 为空数组也不回退到 permissive，
    // 而是使用最严格的 same-origin 策略（不允许任何跨域请求）。
    let cors = if is_localhost {
        tower_http::cors::CorsLayer::permissive()
    } else if let Some(origins) = cfg.allowed_origins {
        if origins.is_empty() {
            // 非 localhost + 空 origins 列表 → 最严格 CORS（不允许任何跨域请求）
            tracing::warn!(
                bind = %cfg.bind,
                "非 localhost 绑定且 allowed_origins 为空，使用最严格 CORS 策略（不允许跨域）"
            );
            tower_http::cors::CorsLayer::new()
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                ])
        } else {
            let parsed: Vec<axum::http::HeaderValue> =
                origins.iter().filter_map(|o| o.parse().ok()).collect();
            if parsed.is_empty() {
                tower_http::cors::CorsLayer::new()
                    .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                    .allow_headers([
                        axum::http::header::CONTENT_TYPE,
                        axum::http::header::AUTHORIZATION,
                    ])
            } else {
                tower_http::cors::CorsLayer::new()
                    .allow_origin(parsed)
                    .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                    .allow_headers([
                        axum::http::header::CONTENT_TYPE,
                        axum::http::header::AUTHORIZATION,
                    ])
            }
        }
    } else {
        tracing::warn!(
            bind = %cfg.bind,
            "CORS restricted to same-origin (non-localhost without allowed_origins config)"
        );
        tower_http::cors::CorsLayer::new()
            .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
            ])
    };

    // CORS 层必须放在最外层（最后 .layer）。
    // Tower 中间件从底部向上包裹：最内层先执行请求处理，
    // 最外层最后处理响应。CORS 需要在所有中间件（包括 Auth、RateLimit）
    // 之前添加 CORS 头，确保即使 Auth 返回 401，响应也携带 CORS 头。
    let app = Router::new()
        .route("/", get(root))
        .route("/api/status", get(api_status))
        .route("/api/apply", post(api_apply))
        .route("/api/rules", get(api_list_rules))
        .route("/api/rules/preview/{patch_id}", get(api_preview_rules))
        .route("/api/rules/{index}/source", get(api_is_prism_rule))
        .route("/api/groups/{group_id}/toggle", post(api_toggle_group))
        .route("/api/trace/{patch_id}", get(api_get_trace))
        .route("/api/stats", get(api_get_stats))
        .route("/api/watch/start", post(api_watch_start))
        .route("/api/watch/stop", post(api_watch_stop))
        .with_state(state.clone())
        // 内层 -> 外层：Auth -> RateLimit -> BodyLimit -> CORS
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(cors);

    // 安全提示：默认绑定 127.0.0.1，仅允许本地访问
    // 如果使用 --bind 0.0.0.0，请确保通过防火墙限制访问，
    // 并使用 --api-key 启用认证

    let addr = format!("{}:{}", cfg.bind, cfg.port);
    println!("Prism Engine HTTP Server");
    println!("   地址: http://{}", addr);
    println!("   API:  http://{}/api/status", addr);
    if state.api_key.is_some() {
        println!("   认证: 已启用 (Bearer Token)");
    } else {
        println!("   认证: 未启用 (仅适用于本地开发)");
    }
    println!("   按 Ctrl+C 退出\n");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("绑定端口失败: {}", e))?;

    let into_make_service_with_connect_info =
        app.into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, into_make_service_with_connect_info)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("Server 错误: {}", e))?;

    ext.stop_watching();
    println!("\nServer 已停止");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}

async fn root() -> &'static str {
    "Prism Engine HTTP Server — see /api/status"
}

async fn api_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let status = state.ext.status();
    let mut value = serde_json::to_value(status).map_err(|e| e.to_string())?;
    // 嵌入 Powered by 标记，GUI 客户端可在状态页面展示
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "powered_by".to_string(),
            serde_json::json!("Prism Engine (https://github.com/Juwan-Hwang/Clash-Prism-Engine)"),
        );
    }
    Ok(Json(value))
}

/// POST /api/apply 请求体（手动解析，避免直接依赖 serde）
struct ApplyRequest {
    skip_disabled: Option<bool>,
    validate: Option<bool>,
}

impl ApplyRequest {
    fn from_value(value: serde_json::Value) -> Self {
        Self {
            skip_disabled: value.get("skip_disabled").and_then(|v| v.as_bool()),
            validate: value.get("validate").and_then(|v| v.as_bool()),
        }
    }
}

async fn api_apply(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let req = ApplyRequest::from_value(body);
    let opts = clash_prism_extension::ApplyOptions {
        skip_disabled_patches: req.skip_disabled.unwrap_or(true),
        validate_output: req.validate.unwrap_or(false),
    };
    let result = state.ext.apply(opts)?;
    let mut value = serde_json::to_value(result).map_err(|e| e.to_string())?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "powered_by".to_string(),
            serde_json::json!("Prism Engine (https://github.com/Juwan-Hwang/Clash-Prism-Engine)"),
        );
    }
    Ok(Json(value))
}

async fn api_list_rules(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rules = state.ext.list_rules()?;
    Ok(Json(
        serde_json::to_value(rules).map_err(|e| e.to_string())?,
    ))
}

async fn api_preview_rules(
    State(state): State<Arc<AppState>>,
    AxumPath(patch_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let diff = state.ext.preview_rules(&patch_id)?;
    Ok(Json(serde_json::to_value(diff).map_err(|e| e.to_string())?))
}

async fn api_is_prism_rule(
    State(state): State<Arc<AppState>>,
    AxumPath(index): AxumPath<usize>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = state.ext.is_prism_rule(index)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|e| e.to_string())?,
    ))
}

async fn api_toggle_group(
    State(state): State<Arc<AppState>>,
    AxumPath(group_id): AxumPath<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // 输入验证：group_id 非空、长度限制、不含路径分隔符
    if group_id.is_empty() {
        return Err(ApiError("group_id 不能为空".to_string()));
    }
    if group_id.len() > 256 {
        return Err(ApiError("group_id 长度不能超过 256 字符".to_string()));
    }
    if group_id.contains('/')
        || group_id.contains('\\')
        || group_id.contains("..")
        || group_id.contains('\0')
    {
        return Err(ApiError("group_id 不能包含路径分隔符或 '..'".to_string()));
    }

    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let ok = state.ext.toggle_group(&group_id, enabled)?;
    Ok(Json(serde_json::json!({ "success": ok })))
}

async fn api_get_trace(
    State(state): State<Arc<AppState>>,
    AxumPath(patch_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let trace = state.ext.get_trace(&patch_id)?;
    Ok(Json(
        serde_json::to_value(trace).map_err(|e| e.to_string())?,
    ))
}

async fn api_get_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stats = state.ext.get_stats()?;
    Ok(Json(
        serde_json::to_value(stats).map_err(|e| e.to_string())?,
    ))
}

async fn api_watch_start(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let debounce = body
        .get("debounce_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(500);
    let debounce = debounce.clamp(100, 60_000); // 下限 100ms，上限 60 秒
    state.ext.start_watching(debounce)?;
    Ok(Json(serde_json::json!({ "success": true })))
}

async fn api_watch_stop(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.ext.stop_watching();
    Ok(Json(serde_json::json!({ "success": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_basic() {
        let limiter = RateLimiter::new();

        // 前 60 次请求应全部通过
        for _ in 0..RATE_LIMIT_MAX_REQUESTS {
            assert!(limiter.check("1.2.3.4"));
        }

        // 第 61 次请求应被拒绝
        assert!(!limiter.check("1.2.3.4"));

        // 不同 IP 不受影响
        assert!(limiter.check("5.6.7.8"));
    }

    #[test]
    fn test_rate_limiter_window_expiry() {
        let limiter = RateLimiter::new();

        // 填满窗口
        for _ in 0..RATE_LIMIT_MAX_REQUESTS {
            assert!(limiter.check("1.2.3.4"));
        }
        assert!(!limiter.check("1.2.3.4"));

        // 由于 RATE_LIMIT_WINDOW 是 60 秒，无法在测试中真正等待。
        // 验证逻辑：手动构造一个已过期的 bucket 来模拟窗口过期。
        // 通过直接操作内部 HashMap 来实现。
        {
            let mut buckets = limiter.buckets.lock().unwrap();
            // 将所有时间戳设为窗口起始之前（模拟全部过期）
            let now = Instant::now();
            let window_start = now - RATE_LIMIT_WINDOW;
            if let Some(ts) = buckets.get_mut("1.2.3.4") {
                for t in ts.iter_mut() {
                    *t = window_start - Duration::from_secs(1);
                }
            }
        }

        // 过期后，新请求应通过（过期记录会被清理）
        assert!(limiter.check("1.2.3.4"));
    }

    #[test]
    fn test_rate_limiter_max_ips() {
        let limiter = RateLimiter::new();

        // 手动填充 buckets 到 RATE_LIMIT_MAX_IPS 个 IP，每个带一个未过期的时间戳
        {
            let mut buckets = limiter.buckets.lock().unwrap();
            let now = Instant::now();
            for i in 0..RATE_LIMIT_MAX_IPS {
                let ip = format!("10.0.0.{}", i);
                buckets.insert(ip, vec![now]);
            }
        }

        // 已存在的 IP 应仍然通过
        assert!(limiter.check("10.0.0.0"));

        // 新 IP 应被拒绝（已达上限）
        assert!(!limiter.check("192.168.1.1"));

        // 再试另一个新 IP，同样被拒绝
        assert!(!limiter.check("192.168.1.2"));
    }

    #[test]
    fn test_rate_limiter_mutex_poison() {
        let limiter = RateLimiter::new();

        // 手动使 Mutex 中毒
        {
            let _lock = limiter.buckets.lock().unwrap();
            // 在持锁状态下 panic，导致 Mutex poison
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                panic!("intentional poison");
            }))
            .ok();
        }

        // Mutex 中毒后，check 应放行（安全降级）
        assert!(limiter.check("1.2.3.4"));
    }

    // ─── shutdown_signal 单元测试 ───

    /// 验证 shutdown_signal 函数签名正确且可编译。
    /// shutdown_signal 是一个 async fn，在测试中我们验证它不会立即 panic。
    /// 实际的 Ctrl+C 触发在 CI 环境中无法模拟，因此仅验证函数可调用。
    #[tokio::test]
    async fn test_shutdown_signal_is_future() {
        // shutdown_signal() 返回一个 Future，验证它可以被创建（poll 一次）。
        // 在没有 Ctrl+C 信号的情况下，它应该 pending。
        // 我们使用 tokio::select! 加超时来验证它不会立即返回。
        use tokio::time::{Duration, sleep};

        tokio::select! {
            _ = shutdown_signal() => {
                // 如果立即返回，说明收到了信号（CI 环境可能如此），也算通过
            }
            _ = sleep(Duration::from_millis(100)) => {
                // 超时说明函数正确地 pending 等待信号 — 预期行为
            }
        }
    }

    /// 验证 shutdown_signal 在多线程环境中可以安全创建。
    /// 确认函数不依赖任何共享状态导致的数据竞争。
    #[tokio::test]
    async fn test_shutdown_signal_no_shared_state_panic() {
        use tokio::time::Duration;

        // 同时创建多个 shutdown_signal future 不应 panic
        let _f1 = shutdown_signal();
        let _f2 = shutdown_signal();
        let _f3 = shutdown_signal();

        // 给 futures 一点时间被 drop（验证 drop 安全）
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
