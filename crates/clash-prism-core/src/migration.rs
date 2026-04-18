//! # 配置迁移系统
//!
//! 参考 Claude Code 的迁移模式：幂等守卫 + 单源读写 + 可追踪。
//!
//! 设计原则：
//! - **幂等性**：每个迁移在 apply 时自行检查是否已执行，重复执行无副作用
//! - **版本守卫**：通过 `prism_version` 字段追踪当前配置版本，仅执行版本号大于当前值的迁移
//! - **有序执行**：按 target_version 升序排列，保证迁移链的确定性
//! - **可审计**：每次迁移返回 `MigrationReport`，包含名称、耗时、是否实际执行等信息
//!
//! 用法：
//! ```ignore
//! let reports = run_migrations(&mut config, &migrations);
//! for r in &reports { println!("{}: {}", r.name, if r.migrated { "已迁移" } else { "跳过" }); }
//! ```

use serde_json::Value;
use std::time::Instant;

/// 迁移结果报告
///
/// 记录单次迁移的执行状态，用于审计和日志输出。
#[derive(Debug, Clone)]
pub struct MigrationReport {
    /// 迁移的唯一名称
    pub name: &'static str,
    /// 是否实际执行了迁移（false 表示已满足，跳过）
    pub migrated: bool,
    /// 耗时（微秒）
    pub duration_us: u64,
    /// 说明信息（如 "已迁移" 或 "版本已满足，跳过"）
    pub message: String,
}

/// 迁移 trait — 每个迁移实现此 trait
///
/// 实现者必须保证 `apply` 的幂等性：如果配置已处于目标状态，
/// 应返回 `Ok(false)` 表示无需修改。
///
/// # Safety & Atomicity
///
/// **实现者必须确保 `apply` 的原子性。** 如果迁移在中途失败（返回 `Err`），
/// 配置可能处于部分修改状态。`run_migrations` 在迁移失败时会**停止后续迁移**
/// 并保留当前版本号不变，但**不会回滚**已部分修改的配置。
///
/// 推荐的原子性实现策略：
/// 1. 先在内存中完成所有修改，验证无误后一次性写入
/// 2. 使用临时变量构建新值，最后用单次 `obj.insert()` 替换
/// 3. 对于复杂迁移，考虑使用备份-恢复模式
///
/// 迁移失败后，管理员需手动检查配置状态并修复问题后重新运行。
pub trait Migration: Send + Sync {
    /// 迁移的唯一名称（用于日志和审计）
    fn name(&self) -> &'static str;

    /// 目标版本号（此迁移将配置从旧版本迁移到此版本）
    fn target_version(&self) -> u32;

    /// 执行迁移，返回是否实际修改了配置
    ///
    /// - `Ok(true)` — 配置已被修改
    /// - `Ok(false)` — 配置已处于目标状态，无需修改
    /// - `Err(msg)` — 迁移失败，返回错误信息
    fn apply(&self, config: &mut Value) -> Result<bool, String>;
}

/// 运行所有已注册的迁移
///
/// 按版本号升序执行，每个迁移幂等（重复执行无副作用）。
/// 仅执行 `target_version > current_version` 的迁移。
///
/// # 参数
/// - `config` — 可变的 JSON 配置，迁移会直接修改此配置
/// - `migrations` — 已注册的迁移列表
///
/// # 返回
/// 实际执行（或跳过）的迁移报告列表
pub fn run_migrations(
    config: &mut Value,
    migrations: &[Box<dyn Migration>],
) -> Vec<MigrationReport> {
    // 1. 读取当前版本号
    let current_version = get_current_version(config);

    // 2. 过滤出需要执行的迁移（target_version > current_version）
    let mut pending: Vec<&Box<dyn Migration>> = migrations
        .iter()
        .filter(|m| m.target_version() > current_version)
        .collect();

    // 3. 按 target_version 升序排序，保证确定性执行顺序
    pending.sort_by_key(|m| m.target_version());

    // 4. 逐个执行迁移，记录报告
    let mut reports = Vec::with_capacity(pending.len());
    let mut latest_version = current_version;

    for migration in pending {
        let start = Instant::now();
        // Deep-copy config before migration for rollback on failure.
        let config_backup = config.clone();
        let result = migration.apply(config);
        let elapsed_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;

        match result {
            Ok(did_migrate) => {
                let report = MigrationReport {
                    name: migration.name(),
                    migrated: did_migrate,
                    duration_us: elapsed_us,
                    message: if did_migrate {
                        format!("已迁移至版本 {}", migration.target_version())
                    } else {
                        "配置已处于目标状态，跳过".to_string()
                    },
                };
                reports.push(report);
                // 无论是否实际修改，都推进版本号
                latest_version = latest_version.max(migration.target_version());
            }
            Err(msg) => {
                // Rollback: restore config from backup on migration failure.
                *config = config_backup;
                let report = MigrationReport {
                    name: migration.name(),
                    migrated: false,
                    duration_us: elapsed_us,
                    message: format!("迁移失败（已回滚）: {}", msg),
                };
                reports.push(report);
                // 迁移失败时不推进版本号，停止后续迁移。
                // 设计决策：如果迁移 V3 失败，版本号停留在 V2，后续 V4/V5 也不会执行。
                // 这确保了迁移链的原子性 — 要么全部成功，要么停留在最后一个已知良好状态。
                // 管理员修复失败原因后，重新运行迁移即可从断点继续。
                break;
            }
        }
    }

    // 5. 更新配置中的版本号
    if latest_version > current_version
        && let Err(e) = set_version(config, latest_version)
    {
        tracing::warn!(error = %e, "迁移完成但版本号更新失败");
    }

    reports
}

/// 获取当前配置版本号
///
/// 从 `config["prism_version"]` 读取，默认为 0。
///
/// 添加范围检查，防止 u64 -> u32 截断导致版本号错误。
/// 如果版本号超过 u32::MAX，记录警告并返回 u32::MAX。
fn get_current_version(config: &Value) -> u32 {
    config
        .get("prism_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .try_into()
        .unwrap_or_else(|_| {
            tracing::warn!(
                version = "prism_version exceeds u32::MAX",
                "Version number too large, clamping to u32::MAX"
            );
            u32::MAX
        })
}

/// 设置配置版本号
///
/// 将 `config["prism_version"]` 设置为指定值。
///
/// 如果 `config` 不是 JSON Object，返回错误而非静默忽略。
fn set_version(config: &mut Value, version: u32) -> Result<(), String> {
    match config.as_object_mut() {
        Some(obj) => {
            obj.insert("prism_version".to_string(), Value::Number(version.into()));
            Ok(())
        }
        None => Err(format!(
            "set_version: config 不是 JSON Object，无法设置 prism_version={}",
            version
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用迁移 V1：添加 new_field 字段
    struct TestMigrationV1;

    impl Migration for TestMigrationV1 {
        fn name(&self) -> &'static str {
            "test_v1"
        }
        fn target_version(&self) -> u32 {
            1
        }
        fn apply(&self, config: &mut Value) -> Result<bool, String> {
            if config.get("new_field").is_some() {
                return Ok(false);
            }
            if let Some(obj) = config.as_object_mut() {
                obj.insert("new_field".into(), Value::String("default".into()));
            }
            Ok(true)
        }
    }

    /// 测试用迁移 V2：添加 another_field 字段
    struct TestMigrationV2;

    impl Migration for TestMigrationV2 {
        fn name(&self) -> &'static str {
            "test_v2"
        }
        fn target_version(&self) -> u32 {
            2
        }
        fn apply(&self, config: &mut Value) -> Result<bool, String> {
            if config.get("another_field").is_some() {
                return Ok(false);
            }
            if let Some(obj) = config.as_object_mut() {
                obj.insert("another_field".into(), Value::Number(42.into()));
            }
            Ok(true)
        }
    }

    /// 测试用迁移 V3：模拟失败的迁移
    struct FailMigrationV3;

    impl Migration for FailMigrationV3 {
        fn name(&self) -> &'static str {
            "fail_v3"
        }
        fn target_version(&self) -> u32 {
            3
        }
        fn apply(&self, _config: &mut Value) -> Result<bool, String> {
            Err("模拟迁移失败".to_string())
        }
    }

    #[test]
    fn test_run_migrations_empty() {
        let mut config = serde_json::json!({});
        let reports = run_migrations(&mut config, &[]);
        assert!(reports.is_empty());
    }

    #[test]
    fn test_run_migrations_applies_new() {
        let mut config = serde_json::json!({});
        let migrations: Vec<Box<dyn Migration>> = vec![Box::new(TestMigrationV1)];
        let reports = run_migrations(&mut config, &migrations);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].migrated);
        assert_eq!(get_current_version(&config), 1);
        assert_eq!(config["new_field"], "default");
    }

    #[test]
    fn test_run_migrations_idempotent() {
        // 版本已满足，应跳过所有迁移
        let mut config = serde_json::json!({"prism_version": 1, "new_field": "default"});
        let migrations: Vec<Box<dyn Migration>> = vec![Box::new(TestMigrationV1)];
        let reports = run_migrations(&mut config, &migrations);
        assert_eq!(reports.len(), 0);
    }

    #[test]
    fn test_run_migrations_sequential() {
        // 空配置，依次执行 V1 和 V2
        let mut config = serde_json::json!({});
        let migrations: Vec<Box<dyn Migration>> =
            vec![Box::new(TestMigrationV2), Box::new(TestMigrationV1)];
        let reports = run_migrations(&mut config, &migrations);
        // 两个迁移都应该执行（按版本号排序后 V1 先，V2 后）
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].name, "test_v1");
        assert_eq!(reports[1].name, "test_v2");
        assert_eq!(get_current_version(&config), 2);
    }

    #[test]
    fn test_run_migrations_partial_apply() {
        // 版本为 1，只应执行 V2
        let mut config = serde_json::json!({"prism_version": 1, "new_field": "default"});
        let migrations: Vec<Box<dyn Migration>> =
            vec![Box::new(TestMigrationV1), Box::new(TestMigrationV2)];
        let reports = run_migrations(&mut config, &migrations);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].name, "test_v2");
        assert!(reports[0].migrated);
        assert_eq!(get_current_version(&config), 2);
    }

    #[test]
    fn test_run_migrations_failure_stops_chain() {
        // 按版本号排序: V1(1) -> V2(2) -> V3(3)
        // V1、V2 成功，V3 失败后停止后续迁移
        let mut config = serde_json::json!({});
        let migrations: Vec<Box<dyn Migration>> = vec![
            Box::new(TestMigrationV1),
            Box::new(FailMigrationV3),
            Box::new(TestMigrationV2),
        ];
        let reports = run_migrations(&mut config, &migrations);
        // V1 执行成功，V2 执行成功，V3 失败
        assert_eq!(reports.len(), 3);
        assert!(reports[0].migrated); // V1
        assert!(reports[1].migrated); // V2
        assert!(!reports[2].migrated); // V3 失败
        // 版本号应停留在 V2（V3 失败不推进）
        assert_eq!(get_current_version(&config), 2);
    }

    #[test]
    fn test_get_current_version_default() {
        let config = serde_json::json!({});
        assert_eq!(get_current_version(&config), 0);
    }

    #[test]
    fn test_set_version() {
        let mut config = serde_json::json!({});
        set_version(&mut config, 5).unwrap();
        assert_eq!(get_current_version(&config), 5);
    }

    #[test]
    fn test_set_version_non_object() {
        let mut config = serde_json::json!("not an object");
        let result = set_version(&mut config, 5);
        assert!(result.is_err());
    }
}
