// AideRun - 原生 SQLite 打包脱敏与隐私重置模块（不依赖外部 Python）

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use windows_sys::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};

use crate::clean::{run_clean, CleanLevel};
use crate::config::CleanupConfig;
use crate::console::{prompt_confirm, to_wide_null};
use crate::paths::PortablePaths;
use crate::process::find_blocking_ide_processes;

const SENSITIVE_EXACT_KEYS: &[&str] = &[
    "antigravityUnifiedStateSync.oauthToken",
    "antigravityUnifiedStateSync.userStatus",
    "antigravityUnifiedStateSync.modelCredits",
    "antigravityUnifiedStateSync.trajectorySummaries",
    "antigravityUnifiedStateSync.sidebarWorkspaces",
    "antigravity.profileUrl",
    "antigravityAnalytics.clearcutBuffer",
    "antigravityAnalytics.lastUploadTime",
];

/// 脱敏执行结果摘要
#[derive(Debug, Clone, Default)]
pub struct SanitizeReport {
    pub db_rows_deleted: usize,
    pub agent_preferences_preserved: bool,
    pub dirs_cleared: usize,
    pub credential_deleted: bool,
}

/// 执行打包前脱敏：清空个人凭据、账号状态、AI 会话历史并保留中文与免权限设置
pub fn run_sanitize(
    paths: &PortablePaths,
    cleanup_cfg: &CleanupConfig,
    full_pack_prep: bool,
    auto_yes: bool,
) -> Result<SanitizeReport> {
    let blockers = find_blocking_ide_processes(&paths.root);
    if !blockers.is_empty() {
        println!("[错误] 检测到 Antigravity IDE 仍在运行，请先关闭后再执行脱敏：");
        for b in &blockers {
            println!("  - PID {:<6} {}", b.pid, b.name);
        }
        anyhow::bail!("存在进程占用，已安全中止脱敏操作。");
    }

    if !auto_yes {
        println!("=== 打包前账号脱敏与环境重置 ===");
        println!("本操作将在保留【简体中文配置】与【免权限规则 (Always Proceed)】的前提下：");
        println!("  1. 彻底清除 Google OAuth Token、用户信息、积分状态、对话摘要与通知历史；");
        println!("  2. 清空 .gemini/antigravity 与 .gemini/antigravity-ide 对话与大脑缓存；");
        println!("  3. 重置匿名 machineid、路径标记并清除系统凭据管理器缓存；");
        if full_pack_prep {
            println!("  4. 同步执行彻底垃圾瘦身 (CleanLevel::All)，直接准备打包分发！");
        }
        if !prompt_confirm("确认执行脱敏处理？[y/N]: ", false) {
            println!("[已取消操作]");
            return Ok(SanitizeReport::default());
        }
    }

    let mut report = SanitizeReport::default();

    if paths.state_vscdb.is_file() {
        let (deleted, kept_pref) = sanitize_state_vscdb(&paths.state_vscdb)?;
        report.db_rows_deleted = deleted;
        report.agent_preferences_preserved = kept_pref;
        println!(
            "  [√] state.vscdb 脱敏完成：已清理 {deleted} 条敏感/冗余记录并压缩数据库（agentPreferences 保留: {}）",
            if kept_pref { "是" } else { "未存在" }
        );
    } else {
        println!("  [-] 未发现 state.vscdb，跳过数据库脱敏。");
    }

    let history_dirs = [
        paths.gemini_dir.join("antigravity-ide").join("conversations"),
        paths.gemini_dir.join("antigravity-ide").join("brain"),
        paths.gemini_dir.join("antigravity").join("conversations"),
        paths.gemini_dir.join("antigravity").join("brain"),
        paths.user_data_dir.join("Backups"),
    ];
    for dir in &history_dirs {
        if dir.is_dir() && clear_dir_recreate(dir).is_ok() {
            report.dirs_cleared += 1;
            println!("  [√] 已清空目录: {}", paths.rel_display(dir));
        }
    }

    for marker_file in [
        paths.user_data_dir.join("machineid"),
        paths.root_marker.clone(),
    ] {
        if marker_file.is_file() && std::fs::remove_file(&marker_file).is_ok() {
            println!("  [√] 已移除设备/路径标记: {}", paths.rel_display(&marker_file));
        }
    }

    if delete_windows_generic_credential("LegacyGeneric:target=gemini:antigravity") {
        report.credential_deleted = true;
        println!("  [√] 已清除 Windows 凭据管理器中的临时凭据");
    }

    if full_pack_prep {
        println!("\n[*] 正在追加执行全量缓存瘦身 (pack-prep)...");
        let _ = run_clean(paths, cleanup_cfg, CleanLevel::All, false, true)?;
    }

    println!("\n[√] 脱敏与环境重置成功！解压后首次启动即可直接登录新账号并沿用简体中文与免权限设置。");
    Ok(report)
}

/// 清理 state.vscdb 中的个人账号及聊天通知冗余，同时保留语言与权限偏好并执行 VACUUM
pub fn sanitize_state_vscdb(db_path: &Path) -> Result<(usize, bool)> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("打开 state.vscdb 失败: {}", db_path.display()))?;

    let mut total_deleted = 0usize;
    {
        let tx = conn.unchecked_transaction()?;
        for key in SENSITIVE_EXACT_KEYS {
            total_deleted += tx.execute("DELETE FROM ItemTable WHERE key = ?1", [key])?;
        }
        total_deleted += tx.execute(
            "DELETE FROM ItemTable WHERE key LIKE 'antigravity.notification.%'",
            [],
        )?;
        tx.commit()?;
    }

    let kept_pref: bool = conn
        .query_row(
            "SELECT COUNT(1) FROM ItemTable WHERE key = 'antigravityUnifiedStateSync.agentPreferences'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    let _ = conn.execute_batch("VACUUM;");
    Ok((total_deleted, kept_pref))
}

fn clear_dir_recreate(dir: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(dir)?;
    std::fs::create_dir_all(dir)
}

fn delete_windows_generic_credential(target_name: &str) -> bool {
    let wide = to_wide_null(target_name);
    unsafe { CredDeleteW(wide.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_sqlite_tokens_while_keeping_agent_preferences() {
        let tmp_db = std::env::temp_dir().join(format!("aiderun_state_{}.vscdb", std::process::id()));
        let _ = std::fs::remove_file(&tmp_db);

        {
            let conn = Connection::open(&tmp_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
                 INSERT INTO ItemTable (key, value) VALUES
                   ('antigravityUnifiedStateSync.oauthToken', 'secret_tok'),
                   ('antigravityUnifiedStateSync.userStatus', 'user_info'),
                   ('antigravityUnifiedStateSync.trajectorySummaries', 'chat_hist'),
                   ('antigravity.notification.abc', 'notify_1'),
                   ('antigravityUnifiedStateSync.agentPreferences', 'always_proceed');",
            )
            .unwrap();
        }

        let (deleted, kept_pref) = sanitize_state_vscdb(&tmp_db).unwrap();
        assert_eq!(deleted, 4);
        assert!(kept_pref);

        let conn = Connection::open(&tmp_db).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM ItemTable", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);

        let _ = std::fs::remove_file(&tmp_db);
    }
}

