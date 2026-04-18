//! 脚本安全限制
//!
//! ## 自定义限制
//!
//! 所有默认值均可通过 [`ScriptRuntime::with_limits()`](crate::ScriptRuntime::with_limits)
//! 自定义覆盖。例如：
//!
//! ```rust,ignore
//! use clash_prism_script::limits::ScriptLimits;
//! use clash_prism_script::ScriptRuntime;
//!
//! let limits = ScriptLimits {
//!     max_execution_time_ms: 10_000,  // 10 秒
//!     max_memory_bytes: 100 * 1024 * 1024,  // 100MB
//!     ..ScriptLimits::default()
//! };
//! let runtime = ScriptRuntime::with_limits(limits);
//! ```

/// 脚本执行安全限制
///
/// 控制脚本运行时的资源上限，防止恶意或低质量脚本耗尽系统资源。
/// 默认值经过平衡设计，适用于绝大多数场景；如有特殊需求，
/// 可通过 [`ScriptRuntime::with_limits()`](crate::ScriptRuntime::with_limits) 自定义。
#[derive(Debug, Clone)]
pub struct ScriptLimits {
    /// 最大执行时间（毫秒）
    pub max_execution_time_ms: u64,

    /// 最大内存（字节）
    pub max_memory_bytes: usize,

    /// 最大输出大小（字节）
    pub max_output_size_bytes: usize,

    /// 最大日志条数
    pub max_log_entries: usize,

    /// Maximum script size (bytes)
    pub max_script_size_bytes: usize,

    /// 最大配置大小（字节，默认 10MB）
    pub max_config_bytes: usize,

    /// 单字符串最大长度（防内存炸弹）
    pub max_string_length: usize,

    /// Maximum loop iterations (防死循环, default 100_000)
    pub max_loop_iterations: u64,

    /// Maximum recursion depth (防栈溢出, default 32)
    pub max_recursion_depth: u32,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            max_execution_time_ms: 5_000,       // 5 秒
            max_memory_bytes: 50 * 1024 * 1024, // 50MB
            max_output_size_bytes: 1024 * 1024, // 1MB
            max_log_entries: 500,
            max_script_size_bytes: 10 * 1024 * 1024, // 10MB
            max_config_bytes: 10 * 1024 * 1024,      // 10MB
            max_string_length: 1024 * 1024,          // 1MB
            max_loop_iterations: 100_000,
            max_recursion_depth: 32,
        }
    }
}
