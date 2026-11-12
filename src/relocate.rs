// AideRun - 便携环境跨盘符零感知自愈引擎（盘符变化检测、多编码路径重写、SQLite 修复与备份回滚）

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;
use rusqlite::{params, types::ValueRef, Connection};
use serde_json::Value;

use crate::config::ToolchainConfig;
use crate::paths::PortablePaths;
use crate::process::find_blocking_ide_processes;
use crate::scoop_heal::auto_heal_scoop_if_needed;

const PORTABLE_ROOT_CHILDREN: &[&str] = &[
    "data",
    "projects",
    "tools",
    "resources",
    "bin",
    "antigravity ide.exe",
    "config.json",
    "aiderun.exe",
];

/// 跨盘符/目录迁移状态检查摘要
#[derive(Debug, Clone)]
pub struct PendingRelocation {
    pub recorded_root: String,
    pub current_root: String,
    pub from_marker: bool,
}

/// 跨盘符自愈执行结果报告
#[derive(Debug, Clone, Default)]
pub struct RelocationReport {
    pub migrated: bool,
    pub old_root: Option<String>,
    pub new_root: String,
    pub files_rewritten: usize,
    pub replacements_applied: usize,
    pub sqlite_rows_updated: usize,
    pub scoop_healed: bool,
    pub skipped_due_to_lock: bool,
    pub warnings: Vec<String>,
}

/// 生成待检测的跨盘符迁移状态（纯只读，供健康诊断展示）
pub fn detect_pending_relocation(paths: &PortablePaths) -> Option<PendingRelocation> {
    if !paths.data_dir.is_dir() {
        return None;
    }
    let current_clean = clean_win_path_str(&paths.root);

    if let Ok(raw_marker) = fs::read_to_string(&paths.root_marker) {
        let recorded = trim_non_root_slash(raw_marker.trim());
        if !recorded.is_empty() {
            if !recorded.eq_ignore_ascii_case(&current_clean) {
                return Some(PendingRelocation {
                    recorded_root: recorded,
                    current_root: current_clean,
                    from_marker: true,
                });
            }
            return None;
        }
    }

    // 若 .portable_root 缺失（例如脱敏打包后首次在新U盘启动），尝试从已知配置文件探测旧根路径
    detect_stale_root_from_configs(paths, &current_clean).map(|recorded| PendingRelocation {
        recorded_root: recorded,
        current_root: current_clean,
        from_marker: false,
    })
}

/// 自动检测并完成便携根目录变更后的无感自愈；未发生盘符变化时毫秒级返回
pub fn auto_relocate_portable_env(
    paths: &PortablePaths,
    toolchain: &ToolchainConfig,
    force: bool,
) -> Result<RelocationReport> {
    let mut report = RelocationReport {
        new_root: clean_win_path_str(&paths.root),
        ..Default::default()
    };

    if !paths.data_dir.is_dir() {
        return Ok(report);
    }

    if !force && !toolchain.auto_heal_relocate {
        return Ok(report);
    }

    let pending = detect_pending_relocation(paths);
    let Some(pending_info) = pending else {
        ensure_root_marker_current(paths);
        if toolchain.auto_heal_scoop && paths.scoop_dir.is_dir() {
            let heal = auto_heal_scoop_if_needed(paths, false)?;
            report.scoop_healed = heal.needed;
            report.warnings.extend(heal.warnings);
        }
        return Ok(report);
    };

    let old_root_str = pending_info.recorded_root;
    let new_root_str = pending_info.current_root;
    report.old_root = Some(old_root_str.clone());

    if !find_blocking_ide_processes(&paths.root).is_empty() {
        report.skipped_due_to_lock = true;
        report.warnings.push(
            "检测到便携目录内仍有 IDE 进程运行，已安全跳过配置文件与数据库迁移重写".to_string(),
        );
        return Ok(report);
    }

    let old_path = PathBuf::from(&old_root_str);
    let new_path = PathBuf::from(&new_root_str);
    let rewriter = RootPathRewriter::new(&old_path, &new_path);

    let targets = collect_relocation_targets(paths);
    let backup_dir = create_backup_snapshot(paths, &targets);

    let migration_result: Result<()> = (|| {
        for target in &targets {
            match target {
                RelocateTarget::JsonFile(p) => {
                    if let Some(count) = rewrite_text_file(p, &rewriter, true)? {
                        if count > 0 {
                            report.files_rewritten += 1;
                            report.replacements_applied += count;
                        }
                    }
                }
                RelocateTarget::PlainTextFile(p) => {
                    if let Some(count) = rewrite_text_file(p, &rewriter, false)? {
                        if count > 0 {
                            report.files_rewritten += 1;
                            report.replacements_applied += count;
                        }
                    }
                }
                RelocateTarget::SqliteDb(p) => {
                    let (rows, repls) = rewrite_state_vscdb(p, &rewriter)?;
                    if rows > 0 {
                        report.files_rewritten += 1;
                        report.sqlite_rows_updated += rows;
                        report.replacements_applied += repls;
                    }
                }
            }
        }
        Ok(())
    })();

    if let Err(err) = migration_result {
        if let Some(ref bdir) = backup_dir {
            restore_backup_snapshot(bdir, &targets);
        }
        return Err(err).context("跨盘符迁移自愈失败，已自动从备份还原原配置");
    }

    if paths.scoop_dir.is_dir() && (toolchain.auto_heal_scoop || toolchain.auto_heal_relocate) {
        match auto_heal_scoop_if_needed(paths, true) {
            Ok(heal) => {
                report.scoop_healed = heal.needed || heal.junctions_repaired > 0 || heal.shims_rewritten > 0;
                report.warnings.extend(heal.warnings);
            }
            Err(err) => {
                report.warnings.push(format!("历史 Scoop 自愈出现警告: {err:#}"));
            }
        }
    }

    ensure_root_marker_current(paths);
    report.migrated = true;
    Ok(report)
}

#[derive(Debug, Clone)]
enum RelocateTarget {
    JsonFile(PathBuf),
    PlainTextFile(PathBuf),
    SqliteDb(PathBuf),
}

impl RelocateTarget {
    fn path(&self) -> &Path {
        match self {
            Self::JsonFile(p) | Self::PlainTextFile(p) | Self::SqliteDb(p) => p.as_path(),
        }
    }
}

fn collect_relocation_targets(paths: &PortablePaths) -> Vec<RelocateTarget> {
    let mut targets = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push_target = |target: RelocateTarget| {
        if !target.path().is_file() {
            return;
        }
        let key = target.path().display().to_string().to_ascii_lowercase();
        if seen.insert(key) {
            targets.push(target);
        }
    };

    // 1. 主扩展目录索引与 User settings / storage / state.vscdb / workspaceStorage
    push_target(RelocateTarget::JsonFile(paths.extensions_dir.join("extensions.json")));
    push_target(RelocateTarget::JsonFile(paths.settings_json.clone()));
    push_user_data_targets(&paths.user_data_dir, &mut push_target);

    // 2. 多开隔离分身 data/instances/<name>/
    let instances_dir = paths.data_dir.join("instances");
    if let Ok(rd) = fs::read_dir(&instances_dir) {
        for entry in rd.flatten() {
            let inst_root = entry.path();
            if !inst_root.is_dir() {
                continue;
            }
            push_target(RelocateTarget::JsonFile(
                inst_root.join("extensions").join("extensions.json"),
            ));
            let inst_user_data = inst_root.join("user-data");
            if inst_user_data.is_dir() {
                push_target(RelocateTarget::JsonFile(
                    inst_user_data.join("User").join("settings.json"),
                ));
                push_user_data_targets(&inst_user_data, &mut push_target);
            }
        }
    }

    // 3. Git 全局凭据与 .gemini MCP 配置
    push_target(RelocateTarget::PlainTextFile(paths.profile_dir.join(".gitconfig")));
    for mcp in [
        paths.gemini_dir.join("antigravity").join("mcp_config.json"),
        paths.gemini_dir.join("antigravity-ide").join("mcp_config.json"),
    ] {
        push_target(RelocateTarget::JsonFile(mcp));
    }

    targets
}

fn push_user_data_targets(user_data_dir: &Path, push_target: &mut impl FnMut(RelocateTarget)) {
    let user_dir = user_data_dir.join("User");
    let global_storage = user_dir.join("globalStorage");
    push_target(RelocateTarget::JsonFile(global_storage.join("storage.json")));
    push_target(RelocateTarget::SqliteDb(global_storage.join("state.vscdb")));

    let ws_storage = user_dir.join("workspaceStorage");
    if let Ok(rd) = fs::read_dir(&ws_storage) {
        for entry in rd.flatten() {
            let ws_dir = entry.path();
            if !ws_dir.is_dir() {
                continue;
            }
            push_target(RelocateTarget::JsonFile(ws_dir.join("workspace.json")));
        }
    }
}

/// 仅保留最近一次备份用于异常即时回滚，避免长期堆积体积
fn create_backup_snapshot(paths: &PortablePaths, targets: &[RelocateTarget]) -> Option<PathBuf> {
    let backup_root = paths.cache_dir.join("_relocate_backup");
    let _ = fs::remove_dir_all(&backup_root);
    let stamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
    let snap_dir = backup_root.join(stamp);
    if fs::create_dir_all(&snap_dir).is_err() {
        return None;
    }

    for (idx, target) in targets.iter().enumerate() {
        let src = target.path();
        if !src.is_file() {
            continue;
        }
        let file_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.bak");
        let dst = snap_dir.join(format!("{idx:03}_{file_name}"));
        let _ = fs::copy(src, dst);
    }
    Some(snap_dir)
}

fn restore_backup_snapshot(snap_dir: &Path, targets: &[RelocateTarget]) {
    for (idx, target) in targets.iter().enumerate() {
        let dst = target.path();
        let file_name = dst
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.bak");
        let bak = snap_dir.join(format!("{idx:03}_{file_name}"));
        if bak.is_file() {
            let _ = fs::copy(&bak, dst);
        }
    }
}

fn ensure_root_marker_current(paths: &PortablePaths) {
    if !paths.data_dir.is_dir() {
        return;
    }
    let desired = format!("{}\n", clean_win_path_str(&paths.root));
    if let Ok(cur) = fs::read_to_string(&paths.root_marker) {
        if cur == desired {
            return;
        }
    }
    let _ = fs::write(&paths.root_marker, desired);
}

fn detect_stale_root_from_configs(paths: &PortablePaths, current_root: &str) -> Option<String> {
    // 1. 从 extensions.json 中提取形如 `X:\...\data\extensions` 之前的根路径
    let ext_json = paths.extensions_dir.join("extensions.json");
    if let Ok(text) = fs::read_to_string(&ext_json) {
        if let Some(found) = extract_root_before_suffix(&text, r"\\data\\extensions")
            .or_else(|| extract_root_before_suffix(&text, r"\data\extensions"))
            .or_else(|| extract_root_before_suffix(&text, "/data/extensions"))
        {
            if !found.eq_ignore_ascii_case(current_root) {
                return Some(found);
            }
        }
    }

    // 2. 从 .gitconfig 中提取形如 `X:\...\tools\` 之前的根路径
    let gitcfg = paths.profile_dir.join(".gitconfig");
    if let Ok(text) = fs::read_to_string(&gitcfg) {
        if let Some(found) = extract_root_before_suffix(&text, r"\\tools\\")
            .or_else(|| extract_root_before_suffix(&text, r"\tools\"))
            .or_else(|| extract_root_before_suffix(&text, "/tools/"))
        {
            if !found.eq_ignore_ascii_case(current_root) {
                return Some(found);
            }
        }
    }

    None
}

fn extract_root_before_suffix(text: &str, marker: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find(&marker.to_ascii_lowercase())?;
    let prefix = &text[..idx];
    let bytes = prefix.as_bytes();

    // 从 marker 位置向左回溯查找 Windows 盘符 `X:`（允许前缀紧邻 `"`, `'`, `/`, 空白等）
    let mut pos = bytes.len();
    while pos >= 2 {
        let i = pos - 2;
        if bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && (i == 0
                || matches!(
                    bytes[i - 1],
                    b'"' | b'\'' | b'/' | b'!' | b'=' | b' ' | b'\t' | b'\n' | b'\r' | b';'
                ))
        {
            let raw = &prefix[i..];
            let unescaped = raw.replace("\\\\", "\\").replace('/', "\\");
            let cleaned = trim_non_root_slash(&unescaped);
            if cleaned.len() >= 2 {
                return Some(cleaned);
            }
        }
        pos -= 1;
    }
    None
}

fn rewrite_text_file(
    path: &Path,
    rewriter: &RootPathRewriter,
    validate_json: bool,
) -> Result<Option<usize>> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let (updated, count) = rewriter.rewrite_text(&raw);
    if count == 0 || updated == raw {
        return Ok(Some(0));
    }
    if validate_json {
        serde_json::from_str::<Value>(updated.trim_start_matches('\u{FEFF}'))
            .with_context(|| format!("重写后 JSON 校验失败，拒绝落盘: {}", path.display()))?;
    }
    atomic_write_file(path, updated.as_bytes())?;
    Ok(Some(count))
}

fn atomic_write_file(target: &Path, content: &[u8]) -> Result<()> {
    let parent = target.parent().context("目标路径无父目录")?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("relocate");
    let tmp = parent.join(format!(".{file_name}.relocate_tmp"));
    fs::write(&tmp, content).with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, target) {
        // Windows 某些场景 rename 覆盖失败时先写目标文件并删除临时文件
        let write_res = fs::write(target, content);
        let _ = fs::remove_file(&tmp);
        write_res.with_context(|| format!("原子覆盖文件失败 ({err}): {}", target.display()))?;
    }
    Ok(())
}

fn rewrite_state_vscdb(db_path: &Path, rewriter: &RootPathRewriter) -> Result<(usize, usize)> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("打开 SQLite 数据库失败: {}", db_path.display()))?;

    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name='ItemTable'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !table_exists {
        return Ok((0, 0));
    }

    let mut updates: Vec<(String, String, usize)> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT key, value FROM ItemTable")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let val_ref = row.get_ref(1)?;
            let raw_bytes: &[u8] = match val_ref {
                ValueRef::Text(b) | ValueRef::Blob(b) => b,
                _ => continue,
            };
            let Ok(text) = std::str::from_utf8(raw_bytes) else {
                continue;
            };
            let trimmed = text.trim();
            let is_json = (trimmed.starts_with('{')
                || trimmed.starts_with('[')
                || trimmed.starts_with('"'))
                && serde_json::from_str::<Value>(trimmed).is_ok();
            // 非 JSON 条目严格禁止包含任何 ASCII 控制字符，彻底避开二进制 Protobuf 记录
            let is_plain_text = !is_json
                && !text.is_empty()
                && !text.bytes().any(|b| b < 0x20);
            if !is_json && !is_plain_text {
                continue;
            }

            let (updated, count) = rewriter.rewrite_text(text);
            if count > 0 && updated != text {
                if is_json && serde_json::from_str::<Value>(updated.trim()).is_err() {
                    continue;
                }
                updates.push((key, updated, count));
            }
        }
    }

    if updates.is_empty() {
        return Ok((0, 0));
    }

    let mut rows_changed = 0usize;
    let mut total_replacements = 0usize;
    let tx = conn.unchecked_transaction()?;
    for (key, new_val, cnt) in updates {
        tx.execute(
            "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
            params![new_val.as_bytes(), key],
        )?;
        rows_changed += 1;
        total_replacements += cnt;
    }
    tx.commit()?;
    Ok((rows_changed, total_replacements))
}

/// 将旧便携根目录映射为新便携根目录（覆盖原生、JSON双反斜杠、正斜杠与 VS Code URI 编码四种形态）
struct RootPathRewriter {
    rules: Vec<PrefixRule>,
}

struct PrefixRule {
    old_prefix: String,
    new_prefix: String,
    sep: &'static str,
    require_portable_child: bool,
    standalone_new: Option<String>,
}

impl RootPathRewriter {
    fn new(old_root: &Path, new_root: &Path) -> Self {
        let old_base = root_base_without_sep(old_root);
        let new_base = root_base_without_sep(new_root);
        let old_is_drive = old_base.len() == 2;
        let new_is_drive = new_base.len() == 2;

        let old_fs = old_base.replace('\\', "/");
        let new_fs = new_base.replace('\\', "/");
        let old_fs_low_drive = lowercase_drive_prefix(&old_fs);
        let new_fs_low_drive = lowercase_drive_prefix(&new_fs);

        let old_uri_encoded_colon = encode_vscode_file_uri(&old_base, true);
        let new_uri_encoded_colon = encode_vscode_file_uri(&new_base, true);
        let old_uri_plain_colon = encode_vscode_file_uri(&old_base, false);
        let new_uri_plain_colon = encode_vscode_file_uri(&new_base, false);

        let standalone_bs = if new_is_drive { Some(format!("{new_base}\\")) } else { None };
        let standalone_bs2 = if new_is_drive { Some(format!("{new_base}\\\\")) } else { None };
        let standalone_fs = if new_is_drive { Some(format!("{new_fs}/")) } else { None };
        let standalone_fs_low = if new_is_drive {
            Some(format!("{new_fs_low_drive}/"))
        } else {
            None
        };
        let standalone_uri_enc = if new_is_drive {
            Some(format!("{new_uri_encoded_colon}/"))
        } else {
            None
        };
        let standalone_uri_plain = if new_is_drive {
            Some(format!("{new_uri_plain_colon}/"))
        } else {
            None
        };

        // 注意顺序：先替换带编码前缀与双反斜杠/URI，再替换单反斜杠/正斜杠
        let rules = vec![
            PrefixRule {
                old_prefix: old_uri_encoded_colon,
                new_prefix: new_uri_encoded_colon,
                sep: "/",
                require_portable_child: old_is_drive,
                standalone_new: standalone_uri_enc,
            },
            PrefixRule {
                old_prefix: old_uri_plain_colon,
                new_prefix: new_uri_plain_colon,
                sep: "/",
                require_portable_child: old_is_drive,
                standalone_new: standalone_uri_plain,
            },
            PrefixRule {
                old_prefix: format!("/{old_fs_low_drive}"),
                new_prefix: format!("/{new_fs_low_drive}"),
                sep: "/",
                require_portable_child: old_is_drive,
                standalone_new: standalone_fs_low.map(|s| format!("/{s}")),
            },
            PrefixRule {
                old_prefix: old_base.replace('\\', "\\\\"),
                new_prefix: new_base.replace('\\', "\\\\"),
                sep: "\\\\",
                require_portable_child: old_is_drive,
                standalone_new: standalone_bs2,
            },
            PrefixRule {
                old_prefix: old_base,
                new_prefix: new_base,
                sep: "\\",
                require_portable_child: old_is_drive,
                standalone_new: standalone_bs,
            },
            PrefixRule {
                old_prefix: old_fs,
                new_prefix: new_fs,
                sep: "/",
                require_portable_child: old_is_drive,
                standalone_new: standalone_fs,
            },
        ];

        Self { rules }
    }

    fn rewrite_text(&self, input: &str) -> (String, usize) {
        let mut current = input.to_string();
        let mut total_hits = 0usize;
        for rule in &self.rules {
            let (next, hits) = apply_prefix_rule(&current, rule);
            if hits > 0 {
                current = next;
                total_hits += hits;
            }
        }
        (current, total_hits)
    }
}

fn apply_prefix_rule(text: &str, rule: &PrefixRule) -> (String, usize) {
    if rule.old_prefix.is_empty()
        || rule.old_prefix.eq_ignore_ascii_case(&rule.new_prefix)
    {
        return (text.to_string(), 0);
    }

    let text_low = text.to_ascii_lowercase();
    let needle_low = rule.old_prefix.to_ascii_lowercase();
    let needle_len = needle_low.len();

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut count = 0usize;

    while cursor < text_low.len() {
        let Some(rel_idx) = text_low[cursor..].find(&needle_low) else {
            out.push_str(&text[cursor..]);
            break;
        };
        let abs_idx = cursor + rel_idx;

        if !has_valid_start_boundary(text.as_bytes(), abs_idx, needle_low.as_bytes()) {
            let advance = abs_idx + next_char_len(&text[abs_idx..]);
            out.push_str(&text[cursor..advance]);
            cursor = advance;
            continue;
        }

        let after_idx = abs_idx + needle_len;
        let rest = &text[after_idx..];

        // 检查紧随前缀后的字符是否为分隔符或结尾边界
        if let Some(after_sep) = rest.strip_prefix(rule.sep) {
            if rule.require_portable_child && !starts_with_portable_child(after_sep) {
                let advance = abs_idx + next_char_len(&text[abs_idx..]);
                out.push_str(&text[cursor..advance]);
                cursor = advance;
                continue;
            }
            out.push_str(&text[cursor..abs_idx]);
            out.push_str(&rule.new_prefix);
            out.push_str(rule.sep);
            count += 1;
            cursor = after_idx + rule.sep.len();
        } else if !rule.require_portable_child && is_end_boundary(rest.as_bytes().first().copied()) {
            out.push_str(&text[cursor..abs_idx]);
            if let Some(ref standalone) = rule.standalone_new {
                out.push_str(standalone);
            } else {
                out.push_str(&rule.new_prefix);
            }
            count += 1;
            cursor = after_idx;
        } else {
            let advance = abs_idx + next_char_len(&text[abs_idx..]);
            out.push_str(&text[cursor..advance]);
            cursor = advance;
        }
    }

    (out, count)
}

fn next_char_len(s: &str) -> usize {
    s.chars().next().map(char::len_utf8).unwrap_or(1)
}

fn has_valid_start_boundary(bytes: &[u8], idx: usize, needle: &[u8]) -> bool {
    if idx == 0 {
        return true;
    }
    // URI / 带前导斜杠 `/d:/...` 规则本身已包含完整协议/前导 `/`
    if needle.starts_with(b"file:///") || needle.starts_with(b"/") {
        let prev = bytes[idx - 1];
        return !prev.is_ascii_alphanumeric();
    }
    let prev = bytes[idx - 1];
    !prev.is_ascii_alphanumeric() && prev != b'\\' && prev != b'/'
}

fn is_end_boundary(next_byte: Option<u8>) -> bool {
    match next_byte {
        None => true,
        Some(b) => matches!(
            b,
            b'"' | b'\'' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']' | b')' | b';' | b'?' | b'#'
        ),
    }
}

fn starts_with_portable_child(rest_after_sep: &str) -> bool {
    let low = rest_after_sep.to_ascii_lowercase();
    PORTABLE_ROOT_CHILDREN.iter().any(|child| {
        if !low.starts_with(child) {
            return false;
        }
        let next = low.as_bytes().get(child.len()).copied();
        next.is_none() || matches!(next, Some(b'\\' | b'/')) || is_end_boundary(next)
    })
}

fn clean_win_path_str(path: &Path) -> String {
    let s = path.display().to_string();
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
    trim_non_root_slash(stripped)
}

fn trim_non_root_slash(raw: &str) -> String {
    let normalized = raw.replace('/', "\\");
    let bytes = normalized.as_bytes();
    if bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return normalized;
    }
    normalized.trim_end_matches('\\').to_string()
}

/// 返回不带末尾分隔符的基础路径串；盘根目录 `D:\` 规范为 `"D:"`
fn root_base_without_sep(path: &Path) -> String {
    let cleaned = clean_win_path_str(path);
    let bytes = cleaned.as_bytes();
    if bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        cleaned[..2].to_string()
    } else {
        cleaned.trim_end_matches('\\').to_string()
    }
}

fn lowercase_drive_prefix(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let mut out = String::with_capacity(s.len());
        out.push((bytes[0] as char).to_ascii_lowercase());
        out.push_str(&s[1..]);
        out
    } else {
        s.to_string()
    }
}

fn encode_vscode_file_uri(base_no_sep: &str, encode_colon: bool) -> String {
    let bytes = base_no_sep.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return format!("file:///{}", encode_uri_segment_slashes(base_no_sep));
    }
    let drive_low = (bytes[0] as char).to_ascii_lowercase();
    let colon = if encode_colon { "%3A" } else { ":" };
    let tail = &base_no_sep[2..].replace('\\', "/");
    if tail.is_empty() {
        format!("file:///{drive_low}{colon}")
    } else {
        format!(
            "file:///{drive_low}{colon}{}",
            encode_uri_segment_slashes(tail)
        )
    }
}

fn encode_uri_segment_slashes(path_tail: &str) -> String {
    let mut out = String::with_capacity(path_tail.len());
    for &b in path_tail.as_bytes() {
        match b {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_all_windows_and_vscode_uri_variants_cleanly() {
        let rewriter = RootPathRewriter::new(
            Path::new(r"D:\Antigravity IDE"),
            Path::new(r"F:\My USB\Work Space"),
        );

        let sample = r#"{
          "fsPath": "d:\\Antigravity IDE\\data\\extensions\\zh-hans",
          "external": "file:///d%3a/Antigravity%20IDE/data/extensions/zh-hans",
          "path": "/d:/Antigravity IDE/data/extensions/zh-hans",
          "backupPath": "D:\\Antigravity IDE\\data\\user-data\\Backups\\abc",
          "recent": "file:///D%3A/Antigravity%20IDE/projects/AideRun",
          "rootUri": "file:///d%3A/Antigravity%20IDE",
          "gitHelper": "!'D:\\Antigravity IDE\\tools\\portable\\gh\\gh.exe' auth git-credential",
          "hostSystem": "C:\\Windows\\System32\\cmd.exe",
          "unrelatedD": "D:\\Antigravity IDE 2\\other"
        }"#;

        let (out, count) = rewriter.rewrite_text(sample);
        assert_eq!(count, 7);
        assert!(out.contains(r"F:\\My USB\\Work Space\\data\\extensions\\zh-hans"));
        assert!(out.contains("file:///f%3A/My%20USB/Work%20Space/data/extensions/zh-hans"));
        assert!(out.contains("/f:/My USB/Work Space/data/extensions/zh-hans"));
        assert!(out.contains("file:///f%3A/My%20USB/Work%20Space/projects/AideRun"));
        assert!(out.contains("\"rootUri\": \"file:///f%3A/My%20USB/Work%20Space\""));
        assert!(out.contains(r"C:\\Windows\\System32\\cmd.exe"));
        assert!(out.contains(r"D:\\Antigravity IDE 2\\other"));
    }

    #[test]
    fn handles_folder_to_drive_root_and_drive_root_to_folder_without_double_slashes() {
        let to_drive_root =
            RootPathRewriter::new(Path::new(r"D:\Antigravity IDE"), Path::new(r"G:\"));
        let (out1, hits1) = to_drive_root.rewrite_text(
            r#"{"fs":"d:\\Antigravity IDE\\data\\extensions","u":"file:///d%3A/Antigravity%20IDE/projects/app","self":"file:///d%3A/Antigravity%20IDE"}"#,
        );
        assert_eq!(hits1, 3);
        assert!(out1.contains(r"G:\\data\\extensions"));
        assert!(out1.contains("file:///g%3A/projects/app"));
        assert!(out1.contains("file:///g%3A/\""));

        let from_drive_root =
            RootPathRewriter::new(Path::new(r"G:\"), Path::new(r"H:\Portable\IDE"));
        let (out2, hits2) = from_drive_root.rewrite_text(
            r#"{"fs":"G:\\data\\extensions","u":"file:///g%3A/projects/app","unrelated":"G:\\Movies\\film.mkv"}"#,
        );
        assert_eq!(hits2, 2);
        assert!(out2.contains(r"H:\\Portable\\IDE\\data\\extensions"));
        assert!(out2.contains("file:///h%3A/Portable/IDE/projects/app"));
        assert!(out2.contains(r"G:\\Movies\\film.mkv"));
    }

    #[test]
    fn auto_relocates_extensions_storage_gitconfig_and_sqlite_safely() {
        let tmp = std::env::temp_dir().join(format!("aiderun_relocate_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        let paths = PortablePaths::from_root(tmp.clone(), tmp.join("AideRun.exe"));
        paths.ensure_skeleton().unwrap();

        let old_root = r"Q:\Antigravity IDE";
        fs::write(&paths.root_marker, format!("{old_root}\n")).unwrap();

        fs::write(
            paths.extensions_dir.join("extensions.json"),
            r#"[{"location":{"fsPath":"q:\\Antigravity IDE\\data\\extensions\\zh","external":"file:///q%3A/Antigravity%20IDE/data/extensions/zh","path":"/q:/Antigravity IDE/data/extensions/zh"}}]"#,
        )
        .unwrap();

        let global_storage = paths.user_data_dir.join("User").join("globalStorage");
        fs::create_dir_all(&global_storage).unwrap();
        fs::write(
            global_storage.join("storage.json"),
            r#"{"backupPath":"Q:\\Antigravity IDE\\data\\user-data\\Backups\\1","openedPathsList":{"entries":[{"folderUri":"file:///q%3A/Antigravity%20IDE/projects/demo"}]}}"#,
        )
        .unwrap();

        fs::write(
            paths.profile_dir.join(".gitconfig"),
            "[credential \"https://github.com\"]\n\thelper = !'Q:\\\\Antigravity IDE\\\\tools\\\\portable\\\\gh\\\\gh.exe' auth git-credential\n",
        )
        .unwrap();

        // 在 state.vscdb 中同时放入可迁移 JSON、纯 URI 文本与一条带 0x0A 控制字节的二进制 Protobuf（不可误伤）
        let db_path = global_storage.join("state.vscdb");
        let protobuf_blob: Vec<u8> = vec![0x0A, 0x05, b'q', b':', b'\\', b'x', b'y'];
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);")
                .unwrap();
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                params![
                    "history.recentlyOpenedPathsList",
                    r#"{"entries":[{"folderUri":"file:///q%3A/Antigravity%20IDE/projects/demo"}]}"#
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                params![
                    "antigravity.icon",
                    "q:/Antigravity IDE/resources/app/logo.svg"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                params!["antigravityUnifiedStateSync.binaryProto", protobuf_blob],
            )
            .unwrap();
        }

        let cfg = ToolchainConfig {
            auto_heal_relocate: true,
            auto_heal_scoop: false,
            ..Default::default()
        };
        let report = auto_relocate_portable_env(&paths, &cfg, false).unwrap();
        assert!(report.migrated);
        assert!(report.files_rewritten >= 4);
        assert_eq!(report.sqlite_rows_updated, 2);

        let new_root_clean = clean_win_path_str(&tmp);
        let ext_updated = fs::read_to_string(paths.extensions_dir.join("extensions.json")).unwrap();
        assert!(ext_updated.contains(&new_root_clean.replace('\\', "\\\\")));
        assert!(!ext_updated.to_ascii_lowercase().contains("q:\\\\antigravity ide"));

        let gitcfg_updated = fs::read_to_string(paths.profile_dir.join(".gitconfig")).unwrap();
        assert!(gitcfg_updated.contains(&new_root_clean.replace('\\', "\\\\")));

        // 二进制 Protobuf 记录必须原封不动保留
        let conn = Connection::open(&db_path).unwrap();
        let stored_proto: Vec<u8> = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = 'antigravityUnifiedStateSync.binaryProto'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_proto, protobuf_blob);

        let marker_now = fs::read_to_string(&paths.root_marker).unwrap();
        assert_eq!(marker_now.trim(), new_root_clean);

        // 二次调用因根路径已一致应直接零开销跳过
        let report2 = auto_relocate_portable_env(&paths, &cfg, false).unwrap();
        assert!(!report2.migrated);
        assert_eq!(report2.files_rewritten, 0);

        let _ = fs::remove_dir_all(&tmp);
    }
}
