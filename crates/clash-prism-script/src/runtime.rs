//! 脚本运行时 — 基于 rquickjs 的安全沙箱
//!
//! ## 架构
//!
//! ```text
//! 用户脚本 (.js)
//!     |
//!     v
//! rquickjs Runtime + Context (sandbox)
//!     |
//!     v
//! Prism API (structured utility functions — §5.2 完整 PrismContext)
//!     |
//!     v
//! clash-prism-core (Patch IR generation)
//! ```

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::api::{KvStore, PatchCollector, ScriptContext};
use crate::limits::ScriptLimits;
use crate::sandbox::SandboxConfig;

/// 脚本执行结果
#[derive(Debug)]
pub struct ScriptResult {
    /// 日志输出
    pub logs: Vec<LogEntry>,

    /// 执行耗时（微秒）
    pub duration_us: u64,

    /// 是否成功
    pub success: bool,

    /// 错误信息（如果失败）
    pub error: Option<String>,

    /// 脚本生成的 Patch 列表（通过 ctx.patch.add() 注册）
    pub patches: Vec<clash_prism_core::ir::Patch>,
}

/// 日志条目
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

/// 脚本运行时
pub struct ScriptRuntime {
    limits: ScriptLimits,
    /// 脚本执行上下文信息
    context: ScriptContext,
    /// KV 存储实例
    kv_store: Arc<KvStore>,
    /// 沙箱安全配置
    sandbox: SandboxConfig,
    /// 递归深度计数器（用于中断处理器中检查递归限制）
    recursion_depth: Arc<AtomicU32>,
    /// 标记步数限制是否已被触发（与 timed_out 独立，用于精确错误消息）
    step_limit_hit: Arc<AtomicBool>,
    /// 标记递归深度限制是否已被触发（与 timed_out 独立，用于精确错误消息）
    recursion_limit_hit: Arc<AtomicBool>,
}

impl ScriptRuntime {
    /// 创建新的脚本运行时（使用默认上下文）
    pub fn new() -> Self {
        Self {
            limits: ScriptLimits::default(),
            context: ScriptContext::default(),
            kv_store: Arc::new(KvStore::new()),
            sandbox: SandboxConfig::strict(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
            step_limit_hit: Arc::new(AtomicBool::new(false)),
            recursion_limit_hit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 使用自定义限制创建运行时
    pub fn with_limits(limits: ScriptLimits) -> Self {
        Self {
            limits,
            context: ScriptContext::default(),
            kv_store: Arc::new(KvStore::new()),
            sandbox: SandboxConfig::strict(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
            step_limit_hit: Arc::new(AtomicBool::new(false)),
            recursion_limit_hit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 使用自定义上下文创建运行时
    pub fn with_context(context: ScriptContext) -> Self {
        Self {
            limits: ScriptLimits::default(),
            context,
            kv_store: Arc::new(KvStore::new()),
            sandbox: SandboxConfig::strict(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
            step_limit_hit: Arc::new(AtomicBool::new(false)),
            recursion_limit_hit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 完全自定义构建
    pub fn with_config(
        limits: ScriptLimits,
        context: ScriptContext,
        kv_store: Arc<KvStore>,
    ) -> Self {
        Self {
            limits,
            context,
            kv_store,
            sandbox: SandboxConfig::strict(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
            step_limit_hit: Arc::new(AtomicBool::new(false)),
            recursion_limit_hit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 使用自定义沙箱配置构建运行时
    pub fn with_sandbox(
        limits: ScriptLimits,
        context: ScriptContext,
        kv_store: Arc<KvStore>,
        sandbox: SandboxConfig,
    ) -> Self {
        Self {
            limits,
            context,
            kv_store,
            sandbox,
            recursion_depth: Arc::new(AtomicU32::new(0)),
            step_limit_hit: Arc::new(AtomicBool::new(false)),
            recursion_limit_hit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Builder 方法：设置沙箱配置（支持链式调用）
    pub fn sandbox(mut self, config: SandboxConfig) -> Self {
        self.sandbox = config;
        self
    }

    /// 获取 KV 存储的共享引用（供外部读写）
    pub fn kv_store(&self) -> Arc<KvStore> {
        Arc::clone(&self.kv_store)
    }

    /// 执行脚本字符串
    ///
    /// # Arguments
    /// * `script` - JavaScript 源代码
    /// * `script_name` - 脚本名称（用于来源追踪）
    /// * `config` - 当前配置（JSON）
    ///
    /// # Returns
    /// 执行结果，包含日志、状态和脚本生成的 Patch
    pub fn execute(
        &self,
        script: &str,
        script_name: &str,
        config: &serde_json::Value,
    ) -> ScriptResult {
        let start = std::time::Instant::now();

        // 生产环境中沙箱安全通过词法级检查和运行时加固代码保证，
        // 此断言作为开发阶段的额外安全网。
        debug_assert!(
            self.sandbox.is_safe(),
            "ScriptRuntime::execute() 要求沙箱处于 strict (safe) 模式。\
             当前配置：allow_network={}, allow_filesystem={}, \
             allow_child_process={}, allow_workers={}",
            self.sandbox.allow_network,
            self.sandbox.allow_filesystem,
            self.sandbox.allow_child_process,
            self.sandbox.allow_workers,
        );

        // 记录沙箱配置（trace 级别）
        tracing::trace!(
            script_name = script_name,
            allow_network = self.sandbox.allow_network,
            allow_filesystem = self.sandbox.allow_filesystem,
            allow_child_process = self.sandbox.allow_child_process,
            allow_workers = self.sandbox.allow_workers,
            permitted_plugins_count = self.sandbox.permitted_plugins.len(),
            "SandboxConfig applied for script execution"
        );

        // 先进行安全验证（词法级安全检查）
        if let Err(e) = self.validate(script) {
            return self.error_result(start, format!("脚本安全验证失败: {}", e));
        }

        // 沙箱感知安全检查：根据沙箱配置动态检测危险模式
        let cleaned_for_sandbox = strip_code_strings_and_comments(script);
        if !self.sandbox.allow_network {
            let network_patterns = [
                ("fetch(", "网络访问被沙箱禁止: fetch"),
                ("XMLHttpRequest", "网络访问被沙箱禁止: XMLHttpRequest"),
                ("WebSocket", "网络访问被沙箱禁止: WebSocket"),
                ("EventSource(", "网络访问被沙箱禁止: EventSource"),
                ("navigator.sendBeacon", "网络访问被沙箱禁止: sendBeacon"),
            ];
            for (pattern, hint) in &network_patterns {
                if cleaned_for_sandbox.contains(pattern) {
                    return self.error_result(start, format!("沙箱安全检查失败: {}", hint));
                }
            }
        }

        // 如果文件系统被禁止，检测文件系统相关 API 调用
        if !self.sandbox.allow_filesystem {
            let fs_patterns = [
                ("require('fs')", "文件系统访问被沙箱禁止: require('fs')"),
                ("require(\"fs\")", "文件系统访问被沙箱禁止: require(\"fs\")"),
                ("import('fs')", "文件系统访问被沙箱禁止: import('fs')"),
            ];
            for (pattern, hint) in &fs_patterns {
                if cleaned_for_sandbox.contains(pattern) {
                    return self.error_result(start, format!("沙箱安全检查失败: {}", hint));
                }
            }
        }

        // 创建 Patch 收集器
        let collector = Arc::new(PatchCollector::new());

        // 1. 创建 rquickjs 运行时和上下文
        let rt = match rquickjs::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => return self.error_result(start, format!("无法创建 JS 运行时: {}", e)),
        };

        // 设置内存限制（架构文档要求 50MB）
        rt.set_memory_limit(self.limits.max_memory_bytes);

        // 设置执行超时中断机制
        // 使用 AtomicBool 作为中断标志，rquickjs 引擎会在执行循环中定期检查
        let timed_out = Arc::new(AtomicBool::new(false));
        let timeout_flag = Arc::clone(&timed_out);
        let timeout_ms = self.limits.max_execution_time_ms;

        // rquickjs 在每个操作码（opcode）执行后调用中断处理器，
        // 因此通过计数器可以精确控制循环迭代和递归深度。
        // 使用 AtomicU64 计数执行步数，超过 max_loop_iterations 时中断。
        let step_counter: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let max_steps = self.limits.max_loop_iterations;
        let step_counter_clone = Arc::clone(&step_counter);
        let max_recursion = self.limits.max_recursion_depth;
        let recursion_depth_clone = Arc::clone(&self.recursion_depth);
        let step_limit_flag = Arc::clone(&self.step_limit_hit);
        let recursion_limit_flag = Arc::clone(&self.recursion_limit_hit);

        let handler: Box<dyn FnMut() -> bool + Send> = Box::new(move || {
            // NOTE (L-13): rquickjs 在每个 opcode 执行后调用此中断处理器，
            // 因此 fetch_add 计数器会在每次 opcode 时触发。
            // AtomicU64::fetch_add 是单条原子指令（x86_64 上为 lock xadd），
            // 开销约 1-2ns，对于安全关键型沙箱环境此性能影响可接受。
            // 检查是否已超时
            if timeout_flag.load(Ordering::Acquire) {
                return true; // 中断执行
            }
            let steps = step_counter_clone.fetch_add(1, Ordering::Relaxed);
            if steps > max_steps {
                step_limit_flag.store(true, Ordering::Release);
                return true; // 超过最大循环迭代次数，中断执行
            }
            if recursion_depth_clone.load(Ordering::Relaxed) > max_recursion {
                recursion_limit_flag.store(true, Ordering::Release);
                return true; // 超过最大递归深度，中断执行
            }
            false
        });
        rt.set_interrupt_handler(Some(handler));

        // 启动超时计时线程：使用 park_timeout + unpark 模式
        // 主线程完成时调用 unpark() 唤醒超时线程，避免 join 阻塞到 sleep 结束
        //
        // 取决于操作系统的调度精度（通常为 1-15ms，Linux 上约 1ms）。
        // 这意味着实际超时时间可能略长于配置值，但在安全方向上（不会提前中断）。
        // 对于需要精确超时的场景，rquickjs 的中断处理器会在每个 opcode 后检查
        // timed_out 标志，提供更精确的执行中断。
        let timeout_handle = {
            let flag = Arc::clone(&timed_out);
            std::thread::spawn(move || {
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    std::thread::park_timeout(remaining);
                    // spurious wakeup 或 unpark() 唤醒后检查是否已到超时
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    // 若被 unpark() 唤醒（主线程完成），flag 已被设为 true，
                    // 此时 remaining > 0 但无需继续 sleep，直接退出
                    if flag.load(Ordering::Acquire) {
                        return;
                    }
                }
                flag.store(true, Ordering::Release);
            })
        };

        let ctx = match rquickjs::Context::full(&rt) {
            Ok(ctx) => ctx,
            Err(e) => {
                // 确保停止超时线程
                timed_out.store(true, Ordering::Release);
                timeout_handle.thread().unpark();
                let _ = timeout_handle.join();
                return self.error_result(start, format!("无法创建 JS 上下文: {}", e));
            }
        };

        let script_logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::with_capacity(64)));
        let logs_for_api = Arc::clone(&script_logs);

        // 2. 在上下文中执行脚本，注册完整 API 并运行
        // 必须克隆 config：execute() 接收 &serde_json::Value 引用，
        // 但 shared_config 需要跨线程（通过 Arc<Mutex<>>）共享给 JS 回调闭包。
        // serde_json::Value 的 clone 是深拷贝，对于大型配置有一定开销，
        // 但这是 &Value → Arc<Mutex<Value>> 所必需的所有权转移。
        let shared_config = Arc::new(std::sync::Mutex::new(config.clone()));
        let result = ctx.with(|ctx| {
            // 注册完整的 PrismContext API（§5.2）
            if let Err(e) = crate::api::PrismApi::register(
                &ctx,
                crate::api::RegisterConfig {
                    config: shared_config,
                    script_ctx: self.context.clone(),
                    kv_store: Arc::clone(&self.kv_store),
                    patch_collector: Arc::clone(&collector),
                    log_collector: logs_for_api,
                    max_log_entries: self.limits.max_log_entries,
                    script_name: script_name.to_string(),
                },
            ) {
                return Err(format!("API 注册失败: {}", e));
            }

            // 即使词法验证被 Unicode/间接调用绕过，运行时也能阻止对危险 API 的访问
            //
            // ## 安全策略（5 层纵深防御）
            //
            // 1. **词法验证**（validate()）：编译前拒绝 eval/Function/require 等危险标识符
            // 2. **删除危险属性**：delete globalThis.eval/Function/require
            // 3. **不可配置属性描述符**：Object.defineProperty 将危险属性设为 accessor（getter/setter 均抛异常）
            // 4. **原型链 constructor 阻断**：对所有内置构造器的 prototype.constructor 设置不可配置 getter
            // 5. **strict mode**：用户脚本在 'use strict' 下执行，禁止 arguments.callee、禁止未声明全局赋值
            //
            // 注意：不使用 Object.freeze(globalThis)，因为 quickjs-ng (rquickjs 0.9+)
            // 中冻结 globalThis 会导致后续的 `var` 声明抛出异常（var 需要在全局对象上
            // 创建属性，但 frozen 对象禁止添加新属性）。
            // Per-property 锁定方案既允许正常变量声明，又能阻止危险属性的重新引入。
            let sandbox_hardening = r#"
                (function() {
                    'use strict';
                    // 在删除 Function 之前保存 Function.prototype 及其 call/apply 的引用。
                    // quickjs-ng 中 delete globalThis.Function 后 Function 变为 undefined，
                    // 导致 Function.prototype 不可访问。保存引用供包装脚本使用。
                    globalThis.__prism_FnProto = Function.prototype;
                    globalThis.__prism_origCall = Function.prototype.call;
                    globalThis.__prism_origApply = Function.prototype.apply;
                    // 删除危险的全局函数和构造器
                    delete globalThis.eval;
                    delete globalThis.Function;
                    delete globalThis.require;
                    // 限制 constructor 链访问（阻止 this.constructor.constructor('return process')() 等攻击）
                    Object.defineProperty(globalThis, 'constructor', {
                        get: function() { throw new Error('Sandbox: constructor access denied'); },
                        configurable: false
                    });
                    // 遍历所有内置构造器，阻断原型链上的 constructor 访问
                    var builtins = ['Object','Array','String','Number','Boolean','RegExp','Date','Error','TypeError','RangeError','SyntaxError','Map','Set','WeakMap','WeakSet','Promise','Symbol','JSON','Math','Reflect','Proxy','ArrayBuffer','DataView','Float32Array','Float64Array','Int8Array','Int16Array','Int32Array','Uint8Array','Uint16Array','Uint32Array','Uint8ClampedArray','BigInt','BigInt64Array','BigUint64Array'];
                    for (var i = 0; i < builtins.length; i++) {
                        try {
                            var ctor = globalThis[builtins[i]];
                            if (typeof ctor === 'function' && ctor.prototype) {
                                Object.defineProperty(ctor.prototype, 'constructor', {
                                    get: function() { throw new Error('Sandbox: constructor access denied'); },
                                    configurable: false
                                });
                            }
                        } catch(e) {}
                    }
                    // Per-property 锁定：对每个危险属性设置不可配置的 accessor property。
                    // getter/setter 均抛异常，configurable: false 由 JS 引擎层面强制执行，
                    // 即使脚本替换 Object.defineProperty 函数本身也无法绕过。
                    //
                    // 列表与 builtins 数组保持语义对齐：
                    // - Node.js 环境危险 API：eval, Function, require, process, module, exports,
                    //   __dirname, __filename, global, Buffer, child_process, fs, net, http, https, dlopen
                    // - 沙箱逃逸向量：WebAssembly, Proxy, Symbol（可用于构造 Reflect.construct 等攻击链）
                    var _dangerous = [
                        'eval','Function','require','process','module','exports',
                        '__dirname','__filename','global','Buffer',
                        'child_process','fs','net','http','https','dlopen',
                        'WebAssembly','Proxy','Symbol'
                    ];
                    for (var k = 0; k < _dangerous.length; k++) {
                        try {
                            Object.defineProperty(globalThis, _dangerous[k], {
                                get: function() { throw new Error('Sandbox: ' + _dangerous[k] + ' is permanently disabled'); },
                                set: function() { throw new Error('Sandbox: re-introducing ' + _dangerous[k] + ' is permanently disabled'); },
                                configurable: false,
                                enumerable: false
                            });
                        } catch(e) {}
                    }
                })()
            "#;
            let harden_result: std::result::Result<(), rquickjs::Error> = ctx.eval(sandbox_hardening);
            if let Err(e) = harden_result {
                tracing::warn!(
                    target = "clash_prism_script",
                    error = %e,
                    "Sandbox hardening 执行失败。\
                     基础防护（词法验证 + Rust 中断处理器）仍然有效。\
                     错误详情: {}",
                    e
                );
            }

            // 包装用户脚本为带递归深度追踪的形式。
            //
            // ## quickjs-ng 兼容性说明
            //
            // 在 quickjs-ng (rquickjs 0.9+) 中，`delete globalThis.Function` 会导致
            // `Function.prototype` 不可访问（Function 变为 undefined）。
            // 因此包装脚本不能直接引用 `Function.prototype.call/apply`。
            //
            // 解决方案：在沙箱加固阶段（Function 被删除之前），将
            // `Function.prototype.call/apply` 保存到全局变量 `__prism_origCall/Apply` 中，
            // 包装脚本通过这些全局变量访问原始方法。
            // 执行完毕后清理这些全局变量。
            //
            // Rust 层的 recursion_depth（AtomicU32）+ 中断处理器仍作为后备机制保留。
            let wrapped_script = format!(
                r#"(function() {{
    'use strict';
    var __maxDepth = {};
    var __currentDepth = 0;
    var __FnProto = globalThis.__prism_FnProto;
    var __origCall = globalThis.__prism_origCall;
    var __origApply = globalThis.__prism_origApply;
    if (typeof __FnProto !== 'undefined' && typeof __origCall === 'function' && typeof __origApply === 'function') {{
        __FnProto.call = function() {{
            __currentDepth++;
            if (__currentDepth > __maxDepth) throw new Error('Maximum recursion depth exceeded (' + __maxDepth + ')');
            try {{ return __origCall.apply(this, arguments); }}
            finally {{ __currentDepth--; }}
        }};
        __FnProto.apply = function(thisArg, args) {{
            __currentDepth++;
            if (__currentDepth > __maxDepth) throw new Error('Maximum recursion depth exceeded (' + __maxDepth + ')');
            try {{ return __origApply.call(this, thisArg, args); }}
            finally {{ __currentDepth--; }}
        }};
    }}
    try {{
        {}
        return {{ success: true }};
    }} catch (e) {{
        return {{ success: false, error: String(e.stack || e.message || e) }};
    }} finally {{
        if (typeof __FnProto !== 'undefined' && typeof __origCall === 'function' && typeof __origApply === 'function') {{
            __FnProto.call = __origCall;
            __FnProto.apply = __origApply;
        }}
    }}
}})()"#,
                self.limits.max_recursion_depth,
                script
            );

            // 使用 Ctx::eval 直接编译并执行字符串
            self.recursion_depth.fetch_add(1, Ordering::Relaxed);
            let eval_result: std::result::Result<rquickjs::Value<'_>, rquickjs::Error> =
                ctx.eval(wrapped_script.as_str());
            self.recursion_depth.fetch_sub(1, Ordering::Relaxed);

            // 检查是否因超时、循环限制或递归深度被中断
            // 优先检查步数和递归标志（由 interrupt handler 精确设置），
            // 再检查超时标志，避免超时与步数/递归超限同时发生时错误消息不精确
            if self.step_limit_hit.load(Ordering::Acquire) {
                let steps = step_counter.load(Ordering::Relaxed);
                return Err(format!(
                    "脚本执行超过最大循环迭代限制 ({} > {} 步)",
                    steps, max_steps
                ));
            }
            if self.recursion_limit_hit.load(Ordering::Acquire) {
                let depth = self.recursion_depth.load(Ordering::Relaxed);
                return Err(format!(
                    "脚本执行超过最大递归深度限制 ({} > {})",
                    depth, max_recursion
                ));
            }
            if timed_out.load(Ordering::Acquire) {
                return Err(format!(
                    "脚本执行超时 ({}ms > {}ms 限制)",
                    start.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    timeout_ms
                ));
            }

            // 清理沙箱加固阶段保存的临时全局变量
            let _: std::result::Result<(), rquickjs::Error> = ctx.eval(
                r#"delete globalThis.__prism_FnProto; delete globalThis.__prism_origCall; delete globalThis.__prism_origApply;"#
            );

            match eval_result {
                Ok(val) => {
                    if let Some(obj) = val.as_object() {
                        let success = obj.get::<_, bool>("success").unwrap_or(false);
                        if !success {
                            let error = obj.get::<_, String>("error").unwrap_or_default();
                            return Err(error);
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(format!("JS 执行错误: {}", e)),
            }
        });

        // 停止超时计时线程：先 unpark 唤醒（使其立即退出 park_timeout），
        // 再 join 等待线程结束（此时 join 几乎立即返回）
        timed_out.store(true, Ordering::Release);
        timeout_handle.thread().unpark();
        let _ = timeout_handle.join();

        let duration_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;

        // 收集脚本生成的 Patch
        let patches = collector.drain_patches();

        match result {
            Ok(()) => {
                let mut logs = match script_logs.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        tracing::warn!(
                            target = "clash_prism_script",
                            "script_logs Mutex 已中毒，正在恢复中毒数据"
                        );
                        poisoned.into_inner()
                    }
                };

                if logs.len() > self.limits.max_log_entries {
                    logs.truncate(self.limits.max_log_entries);
                }

                logs.push(LogEntry {
                    level: LogLevel::Info,
                    message: format!("Script '{}' executed successfully", script_name),
                    timestamp: Utc::now(),
                });

                let output_size: usize = logs
                    .iter()
                    .map(|entry| entry.level.to_string().len() + entry.message.len())
                    .sum::<usize>()
                    + patches
                        .iter()
                        .map(|p| serde_json::to_string(p).map(|s| s.len()).unwrap_or(0))
                        .sum::<usize>();

                if output_size > self.limits.max_output_size_bytes {
                    // 截断日志，保留最后一条说明截断的信息
                    let truncated_msg = format!(
                        "输出大小 {} bytes 超过限制 {} bytes，已截断",
                        output_size, self.limits.max_output_size_bytes
                    );
                    logs.truncate(self.limits.max_log_entries.saturating_sub(1));
                    logs.push(LogEntry {
                        level: LogLevel::Warn,
                        message: truncated_msg,
                        timestamp: Utc::now(),
                    });
                }

                let logs = (*logs).clone();

                ScriptResult {
                    logs,
                    duration_us,
                    success: true,
                    error: None,
                    patches,
                }
            }
            Err(msg) => {
                tracing::error!("Script '{}' failed: {}", script_name, msg);
                ScriptResult {
                    logs: vec![LogEntry {
                        level: LogLevel::Error,
                        message: msg.clone(),
                        timestamp: Utc::now(),
                    }],
                    duration_us,
                    success: false,
                    error: Some(msg),
                    patches,
                }
            }
        }
    }

    /// Validate script static security (without executing it).
    ///
    /// ## Security Model (Lexical State Machine)
    ///
    /// Uses **multi-layer detection** to prevent bypass via string obfuscation:
    ///
    /// ### Layer 1: Raw substring check (fast path)
    /// Catches obvious `eval()`, `require()`, etc.
    ///
    /// ### Layer 2: Lexical token analysis (anti-obfuscation)
    /// Strips strings/comments first, then checks for dangerous tokens.
    /// This catches: `const e='eval'; this[e](...)`, `` const x = "ev" + "al" ``, etc.
    ///
    /// ### Layer 3: Bracket-access property detection
    /// Catches `this["eval"](...)`, `globalThis["Function"](...)`.
    ///
    /// ### Layer 4: Template literal construction
    /// Catches `` `ev` + `al` `` patterns used to build forbidden names.
    pub fn validate(&self, script: &str) -> std::result::Result<(), String> {
        // Size check
        if script.len() > self.limits.max_script_size_bytes {
            return Err(format!(
                "脚本大小超限: {} bytes > {} bytes",
                script.len(),
                self.limits.max_script_size_bytes
            ));
        }

        // 将转义序列转换为实际字符后再进行后续所有层的检测。
        let script: Box<str> = preprocess_unicode_escapes(script).into_boxed_str();
        let script: &str = &script;

        // ═══ Layer 1: Raw substring check (fast path for obvious violations) ═══
        // 因为加固代码（sandbox_hardening）依赖 globalThis 来删除危险属性和冻结全局对象。
        // globalThis 的危险使用由 Layer 2 词法分析和运行时加固代码共同防护。
        let raw_dangerous = [
            ("import ", "不允许使用 import（无模块加载）"),
            ("require(", "不允许使用 require（无模块加载）"),
            ("eval(", "不允许使用 eval（安全隐患）"),
            ("Function(", "不允许使用 Function 构造器（安全隐患）"),
            ("process.", "不允许访问 process 对象"),
            ("__proto__", "不允许访问 __proto__"),
            ("constructor[", "不允许通过 constructor 访问原型链"),
        ];

        for (pattern, hint) in &raw_dangerous {
            if script.contains(pattern) {
                return Err(format!("检测到危险代码 '{}': {}", pattern, hint));
            }
        }

        // ═══ Layer 2: Lexical token analysis (string/comment-stripped) ═══
        // Strip string literals and comments, then check the remaining "pure code"
        // This defeats: const e='eval'; this[e](...)
        let cleaned = strip_code_strings_and_comments(script);

        // ═══ Bracket balance check ═══
        {
            let mut parens: i32 = 0;
            let mut brackets: i32 = 0;
            let mut braces: i32 = 0;
            for (idx, ch) in cleaned.char_indices() {
                match ch {
                    '(' => parens += 1,
                    ')' => {
                        parens -= 1;
                        if parens < 0 {
                            return Err(format!("不平衡的圆括号: 在位置 {} 发现多余的 ')'", idx));
                        }
                    }
                    '[' => brackets += 1,
                    ']' => {
                        brackets -= 1;
                        if brackets < 0 {
                            return Err(format!("不平衡的方括号: 在位置 {} 发现多余的 ']'", idx));
                        }
                    }
                    '{' => braces += 1,
                    '}' => {
                        braces -= 1;
                        if braces < 0 {
                            return Err(format!("不平衡的花括号: 在位置 {} 发现多余的 '}}'", idx));
                        }
                    }
                    _ => {}
                }
            }
            if parens > 0 {
                return Err(format!("不平衡的圆括号: 缺少 {} 个 ')'", parens));
            }
            if brackets > 0 {
                return Err(format!("不平衡的方括号: 缺少 {} 个 ']'", brackets));
            }
            if braces > 0 {
                return Err(format!("不平衡的花括号: 缺少 {} 个 '}}'", braces));
            }
        }

        // Dangerous identifiers that must not appear even as variable/function names
        // Suggestion 注释说明：
        // `exec`、`execSync`、`execFile`、`spawn`、`child_process`、`fs.`、`path.`、
        // `net.`、`http.`、`https.`、`dlopen` 是 Node.js 特有的 API。
        // 在 rquickjs 环境中这些标识符不会匹配到任何运行时对象（rquickjs 不提供 Node.js API），
        // 但保留作为纵深防御：如果未来运行时环境变化（如嵌入 Node.js），这些检查可以防止绕过。
        let lexical_forbidden = [
            "eval",
            "Function",
            "require",
            "import",
            "process",
            "globalThis",
            "__proto__",
            "constructor",
            "spawn",
            "exec",
            "execSync",
            "execFile",
            "child_process",
            "fs.",
            "path.",
            "net.",
            "http.",
            "https.",
            "dlopen",
            "WebAssembly",
            "wasm",
            "Reflect.construct",
            "Reflect.set",
            "Reflect.apply",
            "Reflect.defineProperty",
            "Reflect.setPrototypeOf",
        ];

        for &token in &lexical_forbidden {
            // Use word-boundary-aware check on cleaned code
            if contains_token_as_word(&cleaned, token) {
                return Err(format!(
                    "检测到危险标识符 '{}'（词法分析层，绕过字符串混淆无效）",
                    token
                ));
            }
        }

        // ═══ Layer 3: Bracket-access property detection ═══
        // Catches this["eval"], this["Function"], globalThis["eval"], etc.
        // Check original script (strings inside brackets are meaningful here)
        static BRACKET_ACCESS: OnceLock<regex::Regex> = OnceLock::new();
        let bracket_access = BRACKET_ACCESS.get_or_init(|| {
            regex::Regex::new(r#"\[(?:"|')(?:eval|Function|process|require|import|__proto__|spawn|exec|child_process|fs|dlopen|WebAssembly)(?:"|')\]"#).expect("BRACKET_ACCESS regex compilation must succeed")
        });
        if bracket_access.is_match(script) {
            return Err("检测到方括号属性访问尝试调用危险函数（如 this['eval']）".into());
        }

        // ═══ Layer 4: Template literal construction detection ═══
        // 检测 `${...eval...}` 或 `${...Function...}` 等模式（在 `${}` 内部检查是否包含危险标识符）
        static TEMPLATE_CONSTRUCT: OnceLock<regex::Regex> = OnceLock::new();
        let template_construct = TEMPLATE_CONSTRUCT.get_or_init(|| {
            regex::Regex::new(
                r#"`[^`]*\$\{[^}]*?(?:eval|Function|process|require|import|__proto__|spawn|exec|child_process|dlopen|WebAssembly)[^}]*?\}[^`]*`"#
            ).expect("TEMPLATE_CONSTRUCT regex compilation must succeed")
        });
        if template_construct.is_match(script) {
            return Err("检测到模板字面量中可能包含危险函数调用".into());
        }

        // ═══ Layer 4.5: Template literal concatenation detection ═══
        // Detects patterns like `ev` + `al` which construct dangerous identifiers
        // by concatenating template literals (or string literals) with the `+` operator.
        // 例如：`e` + `v` + `a` + `l` → "eval"
        static TEMPLATE_CONCAT: OnceLock<regex::Regex> = OnceLock::new();
        let template_concat = TEMPLATE_CONCAT.get_or_init(|| {
            regex::Regex::new(
                r#"(?:`[^`]{1,20}`|'[^']{1,20}'|"[^"]{1,20}")\s*\+\s*(?:`[^`]{1,20}`|'[^']{1,20}'|"[^"]{1,20}")"#
            ).expect("TEMPLATE_CONCAT regex compilation must succeed")
        });

        // 迭代合并：反复查找并替换拼接模式，直到无法再合并
        // 每次迭代将 "str1" + "str2" 替换为 "str1str2"，最多迭代 10 次
        let mut merged_script = script.to_string();
        for _ in 0..10 {
            if let Some(caps) = template_concat.captures(&merged_script) {
                let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                let parts: Vec<&str> = full_match.split('+').collect();
                let concatenated: String = parts
                    .iter()
                    .map(|p| {
                        p.trim()
                            .trim_start_matches('`')
                            .trim_end_matches('`')
                            .trim_start_matches('\'')
                            .trim_end_matches('\'')
                            .trim_start_matches('"')
                            .trim_end_matches('"')
                    })
                    .collect();
                // 将合并结果替换回脚本（用引号包裹以保持语法合法性）
                let replacement = format!("\"{}\"", concatenated);
                merged_script = merged_script.replacen(full_match, &replacement, 1);
            } else {
                break;
            }
        }

        // 对最终合并后的脚本检查是否包含危险标识符
        let dangerous_targets = [
            "eval",
            "Function",
            "process",
            "require",
            "import",
            "__proto__",
            "constructor",
            "spawn",
            "exec",
            "child_process",
            "dlopen",
            "WebAssembly",
            "wasm",
            "Reflect",
        ];
        for target in &dangerous_targets {
            // 在合并后的脚本中检查是否出现了完整的危险标识符
            // 使用 contains_token_as_word 确保是完整标识符而非子串
            if contains_token_as_word(&merged_script, target) {
                return Err(format!(
                    "检测到通过字符串/模板字面量拼接构造危险标识符 '{}'",
                    target
                ));
            }
        }

        // Single-line length check (prevent memory bombs)
        for line in script.lines() {
            if line.len() > self.limits.max_string_length {
                return Err(format!(
                    "单行超长: {} chars > {} chars",
                    line.len(),
                    self.limits.max_string_length
                ));
            }
        }

        // 预估拼接结果长度是否超过 max_string_length
        static REPEAT_PATTERN: OnceLock<regex::Regex> = OnceLock::new();
        let repeat_re = REPEAT_PATTERN.get_or_init(|| {
            regex::Regex::new(r#"\.repeat\(\s*(\d+)\s*\)"#)
                .expect("REPEAT_PATTERN regex compilation must succeed")
        });
        for cap in repeat_re.captures_iter(script) {
            if let Some(m) = cap.get(1) {
                if let Ok(count) = m.as_str().parse::<usize>() {
                    // .repeat(N) 的结果长度 = 基础字符串长度 × N
                    // 保守估计：假设基础字符串至少 1 字符
                    if count > self.limits.max_string_length {
                        return Err(format!(
                            "字符串 .repeat({}) 将产生超长结果 (> {} chars)",
                            count, self.limits.max_string_length
                        ));
                    }
                }
            }
        }

        static ARRAY_FILL_PATTERN: OnceLock<regex::Regex> = OnceLock::new();
        let array_fill_re = ARRAY_FILL_PATTERN.get_or_init(|| {
            regex::Regex::new(r#"Array\(\s*(\d+)\s*\)"#)
                .expect("ARRAY_FILL_PATTERN regex compilation must succeed")
        });
        for cap in array_fill_re.captures_iter(script) {
            if let Some(m) = cap.get(1) {
                if let Ok(size) = m.as_str().parse::<usize>() {
                    if size > self.limits.max_string_length {
                        return Err(format!(
                            "Array({}) 将分配超大数组 (> {} 元素)",
                            size, self.limits.max_string_length
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// 获取当前限制配置
    pub fn limits(&self) -> &ScriptLimits {
        &self.limits
    }

    /// 获取当前执行上下文
    pub fn context(&self) -> &ScriptContext {
        &self.context
    }

    /// 获取当前沙箱配置
    pub fn sandbox_config(&self) -> &SandboxConfig {
        &self.sandbox
    }

    /// 构建一个错误结果的快捷方法
    fn error_result(&self, start: std::time::Instant, message: String) -> ScriptResult {
        ScriptResult {
            logs: vec![LogEntry {
                level: LogLevel::Error,
                message: message.clone(),
                timestamp: Utc::now(),
            }],
            duration_us: start.elapsed().as_micros().min(u64::MAX as u128) as u64,
            success: false,
            error: Some(message),
            patches: vec![],
        }
    }
}

impl Default for ScriptRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ══════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════

/// 将 JavaScript 源代码中的 `\uXXXX` 和 `\xXX` 转义序列转换为实际 Unicode 字符。
///
/// 此预处理步骤防止攻击者通过 Unicode 转义绕过安全检查，例如：
/// - `\u0065val` → `eval`
/// - `\x65val` → `eval`
///
/// 注意：此函数仅处理字符串字面量和注释之外的转义序列。
/// 在字符串字面量内部的转义序列不应被转换（它们是合法的字符串内容）。
/// 但为了安全起见，我们对整个脚本进行转换——如果转换后的代码包含危险标识符，
/// 说明攻击者试图通过转义绕过检测。
fn preprocess_unicode_escapes(script: &str) -> String {
    let mut result = String::with_capacity(script.len());
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            // 双反斜杠 `\\` → 输出单个 `\`，跳过两个字符。
            // 这防止 `\\u0065val` 被错误转换为 `\eval`（应为 `\` + `u0065val`）。
            if chars[i + 1] == '\\' {
                result.push('\\');
                i += 2;
                continue;
            }
            if chars[i + 1] == 'u' && i + 5 < chars.len() {
                // \uXXXX 格式
                let hex: String = chars[i + 2..i + 6].iter().collect();
                if let Ok(code_point) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(code_point) {
                        result.push(c);
                        i += 6;
                        continue;
                    }
                }
            } else if chars[i + 1] == 'u' && i + 5 < chars.len() && chars[i + 2] == '{' {
                // \u{XXXXX} 格式（ES6 extended）
                let end_brace = chars[i + 3..].iter().position(|&c| c == '}');
                if let Some(pos) = end_brace {
                    let hex: String = chars[i + 3..i + 3 + pos].iter().collect();
                    if let Ok(code_point) = u32::from_str_radix(&hex, 16) {
                        if let Some(c) = char::from_u32(code_point) {
                            result.push(c);
                            i += 3 + pos + 1;
                            continue;
                        }
                    }
                }
            } else if chars[i + 1] == 'x' && i + 3 < chars.len() {
                // \xXX 格式
                let hex: String = chars[i + 2..i + 4].iter().collect();
                if let Ok(byte_val) = u8::from_str_radix(&hex, 16) {
                    result.push(byte_val as char);
                    i += 4;
                    continue;
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

// ══════════════════════════════════════════════════════════
// Lexical Security Helpers
// ══════════════════════════════════════════════════════════

/// Strip string literals and comments from JavaScript source code,
/// returning the "pure code" that contains only executable tokens.
///
/// This defeats obfuscation like:
/// ```javascript
/// const e = 'eval';   // 'eval' is inside a string → stripped
/// this[e]('...');      // e is a variable, but eval won't appear in cleaned output
/// // however: if someone writes `eval(...)` directly, it survives
/// ```
///
/// The key insight: after stripping strings/comments, any remaining
/// occurrence of `eval`/`Function` etc. means it's **actual code**, not
/// a string value.
/// Strip string literals, comments, template literals, and regex literals from JS source code.
///
/// Returns "pure code" with all string/comment content replaced by spaces.
/// This is the core of Layer 2 lexical analysis — after stripping,
/// any remaining occurrence of `eval`/`Function` etc. is **actual code**, not a string value.
///
/// ## Note on duplication
///
/// A similar function exists in `clash-prism-dsl/src/parser.rs` (`strip_strings_and_comments`).
/// The two implementations have intentional differences:
/// - **This version** (runtime.rs): Handles **regex literals** (`/pattern/flags`) because
///   script validation needs to detect dangerous content inside regex patterns too.
/// - **DSL version** (parser.rs): Uses `match` instead of `if` chains, no regex literal
///   handling (DSL expressions don't contain regex literals).
///
/// 未来可考虑将两套实现统一为一个共享模块中的函数，
/// 通过参数控制是否处理 regex literal，避免重复维护。
fn strip_code_strings_and_comments(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Single-line comment
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            // Skip until end of line
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Multi-line comment
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2; // skip */
            continue;
        }

        // String literal (single-quote)
        if chars[i] == '\'' || chars[i] == '"' {
            let quote = chars[i];
            i += 1;
            // Replace string content with spaces (preserve length for position accuracy)
            result.push(' ');
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    result.push(' '); // escaped char
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    i += 1; // closing quote
                    break;
                }
                result.push(' ');
                i += 1;
            }
            result.push(' ');
            continue;
        }

        // Template literal (backtick)
        if chars[i] == '`' {
            i += 1;
            result.push(' ');
            let mut template_depth: u32 = 0;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    result.push(' ');
                    i += 2;
                    continue;
                }
                // 进入 ${ 时增加深度
                if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                    template_depth += 1;
                    result.push(' ');
                    result.push(' ');
                    i += 2;
                    continue;
                }
                // } 减少深度，仅当 depth > 0 时（避免匹配普通对象字面量）
                if chars[i] == '}' && template_depth > 0 {
                    template_depth -= 1;
                    result.push(' ');
                    i += 1;
                    continue;
                }
                // 仅当不在 ${} 内时，反引号才结束模板字面量
                if chars[i] == '`' && template_depth == 0 {
                    i += 1;
                    break;
                }
                result.push(' ');
                i += 1;
            }
            result.push(' ');
            continue;
        }

        // Regular expression literal
        if chars[i] == '/'
            && !result.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == ')')
        {
            i += 1;
            result.push(' ');
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    result.push(' ');
                    i += 2;
                    continue;
                }
                if chars[i] == '/' {
                    i += 1;
                    // Check for flags (g, i, m, s, u, y)
                    while i < chars.len() && matches!(chars[i], 'g' | 'i' | 'm' | 's' | 'u' | 'y') {
                        result.push(' ');
                        i += 1;
                    }
                    break;
                }
                result.push(' ');
                i += 1;
            }
            result.push(' ');
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Check whether `token` appears as a whole word (identifier boundary)
/// in the given text.
///
/// A "word" is defined as being preceded/followed by non-identifier characters.
/// JavaScript identifier characters: `[a-zA-Z0-9_$]`.
///
/// Uses byte-level operations throughout to avoid byte-offset vs char-index
/// confusion with non-ASCII text (e.g. Chinese characters are multi-byte).
fn contains_token_as_word(text: &str, token: &str) -> bool {
    let text_bytes = text.as_bytes();
    let token_bytes = token.as_bytes();
    let mut search_start = 0;

    while search_start <= text_bytes.len().saturating_sub(token_bytes.len()) {
        if let Some(pos) = text_bytes[search_start..]
            .windows(token_bytes.len())
            .position(|w| w == token_bytes)
        {
            let abs_pos = search_start + pos;
            let end_pos = abs_pos + token_bytes.len();

            // Check character boundary before token
            let valid_before = if abs_pos == 0 {
                true
            } else {
                let before = text_bytes[abs_pos - 1];
                !is_id_byte(before)
            };

            // Check character boundary after token
            let valid_after = if end_pos >= text_bytes.len() {
                true
            } else {
                let after = text_bytes[end_pos];
                !is_id_byte(after)
            };

            if valid_before && valid_after {
                return true;
            }

            search_start = abs_pos + 1;
        } else {
            break;
        }
    }
    false
}

/// Check if a byte is an identifier character (ASCII only).
/// Used by `contains_token_as_word` for byte-level word-boundary detection,
/// avoiding byte-offset vs char-index confusion with non-ASCII text.
fn is_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::ScriptLimits;

    // ═══ Layer 1: Raw substring check ═══

    #[test]
    fn validate_rejects_eval() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("eval('x')").is_err());
    }

    #[test]
    fn validate_rejects_require() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("require('fs')").is_err());
    }

    #[test]
    fn validate_rejects_new_function() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("new Function('return 1')").is_err());
    }

    #[test]
    fn validate_rejects_import_with_space() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("import('fs')").is_err());
    }

    // ═══ Layer 2: Lexical token analysis ═══

    #[test]
    fn validate_rejects_process_exit() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("process.exit()").is_err());
    }

    #[test]
    fn validate_rejects_globalthis() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("globalThis.eval").is_err());
    }

    #[test]
    fn validate_rejects_tab_after_import() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("import\t('fs')").is_err());
    }

    #[test]
    fn validate_rejects_require_in_assignment() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("const fs = require('fs')").is_err());
    }

    #[test]
    fn validate_rejects_eval_after_comment() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("// comment\neval('x')").is_err());
    }

    #[test]
    fn validate_rejects_string_obfuscated_eval() {
        let rt = ScriptRuntime::new();
        // "const e = 'eval'; e" — after string stripping, the lexical analyzer
        // detects the obfuscated eval pattern and rejects it.
        let result = rt.validate("const e = 'eval'; e");
        assert!(
            result.is_err(),
            "String-obfuscated eval should be rejected by lexical analysis"
        );
    }

    // ═══ Layer 3: Bracket-access property detection ═══

    #[test]
    fn validate_rejects_bracket_access_eval() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("this[\"eval\"]").is_err());
    }

    // ═══ Layer 4: Template literal construction ═══

    #[test]
    fn validate_rejects_template_literal_construction() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("`ev` + `al`").is_err());
    }

    // ═══ Safe code should pass ═══

    #[test]
    fn validate_allows_safe_math() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("1 + 2").is_ok());
    }

    #[test]
    fn validate_allows_safe_string() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("'hello'.toUpperCase()").is_ok());
    }

    #[test]
    fn validate_allows_variable_declaration() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("let x = 42").is_ok());
    }

    // ═══ Construction & limits ═══

    #[test]
    fn new_runtime_has_default_limits() {
        let rt = ScriptRuntime::new();
        let limits = rt.limits();
        assert_eq!(limits.max_execution_time_ms, 5_000);
        assert_eq!(limits.max_memory_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn with_limits_custom_limits() {
        let custom = ScriptLimits {
            max_execution_time_ms: 1_000,
            max_memory_bytes: 1024,
            max_output_size_bytes: 512,
            max_log_entries: 10,
            max_script_size_bytes: 2048,
            max_config_bytes: 5 * 1024 * 1024,
            max_string_length: 256,
            max_loop_iterations: 50_000,
            max_recursion_depth: 16,
        };
        let rt = ScriptRuntime::with_limits(custom.clone());
        assert_eq!(rt.limits().max_execution_time_ms, 1_000);
        assert_eq!(rt.limits().max_memory_bytes, 1024);
        assert_eq!(rt.limits().max_loop_iterations, 50_000);
        assert_eq!(rt.limits().max_recursion_depth, 16);
    }

    #[test]
    fn script_limits_default_all_8_fields() {
        let limits = ScriptLimits::default();
        assert_eq!(limits.max_execution_time_ms, 5_000);
        assert_eq!(limits.max_memory_bytes, 50 * 1024 * 1024);
        assert_eq!(limits.max_output_size_bytes, 1024 * 1024);
        assert_eq!(limits.max_log_entries, 500);
        assert_eq!(limits.max_script_size_bytes, 10 * 1024 * 1024);
        assert_eq!(limits.max_string_length, 1024 * 1024);
        assert_eq!(limits.max_loop_iterations, 100_000);
        assert_eq!(limits.max_recursion_depth, 32);
    }

    // ═══ Adversarial edge cases ═══

    #[test]
    fn validate_rejects_script_too_large() {
        let custom = ScriptLimits {
            max_script_size_bytes: 10,
            ..ScriptLimits::default()
        };
        let rt = ScriptRuntime::with_limits(custom);
        let big_script = "12345678901"; // 11 bytes > 10
        let err = rt.validate(big_script).unwrap_err();
        assert!(err.contains("脚本大小超限"));
    }

    #[test]
    fn validate_rejects_line_too_long() {
        let custom = ScriptLimits {
            max_string_length: 5,
            ..ScriptLimits::default()
        };
        let rt = ScriptRuntime::with_limits(custom);
        let long_line = "123456"; // 6 chars > 5
        let err = rt.validate(long_line).unwrap_err();
        assert!(err.contains("单行超长"));
    }

    #[test]
    fn validate_rejects_constructor_bracket() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("constructor['eval']").is_err());
    }

    #[test]
    fn validate_rejects_proto() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("__proto__").is_err());
    }

    #[test]
    fn validate_rejects_spawn() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("spawn('ls')").is_err());
    }

    #[test]
    fn validate_rejects_exec() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("exec('whoami')").is_err());
    }

    #[test]
    fn validate_rejects_webassembly() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("WebAssembly.compile(bytes)").is_err());
    }

    #[test]
    fn validate_rejects_dlopen() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("process.dlopen(module)").is_err());
    }

    #[test]
    fn validate_allows_safe_computation() {
        let rt = ScriptRuntime::new();
        let safe = r#"
            let sum = 0;
            for (let i = 0; i < 10; i++) {
                sum += i;
            }
            let result = sum * 2;
        "#;
        assert!(rt.validate(safe).is_ok());
    }

    #[test]
    fn validate_allows_array_operations() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("[1, 2, 3].map(x => x * 2)").is_ok());
    }

    #[test]
    fn validate_rejects_multiline_comment_obfuscated_eval() {
        let rt = ScriptRuntime::new();
        // eval hidden inside a multi-line comment should be stripped
        // but eval after the comment should be caught
        assert!(rt.validate("/* eval is safe here */\neval('x')").is_err());
    }

    #[test]
    fn validate_rejects_double_quote_bracket_access() {
        let rt = ScriptRuntime::new();
        assert!(rt.validate("this['Function']").is_err());
    }

    #[test]
    fn default_runtime_uses_strict_sandbox() {
        let rt = ScriptRuntime::new();
        let sb = rt.sandbox_config();
        assert!(!sb.allow_network);
        assert!(!sb.allow_filesystem);
        assert!(!sb.allow_child_process);
        assert!(!sb.allow_workers);
    }
}
