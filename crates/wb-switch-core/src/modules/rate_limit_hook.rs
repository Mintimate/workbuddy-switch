//! 限额 hook 的安装 / 卸载 / 状态：把 `Stop` + `FinalStop` 两个事件注册进
//! 三处客户端配置（`~/.codebuddy`、`~/.workbuddy`、`~/.workbuddy-ai` 的 `settings.json`）。
//!
//! 只注册这两个事件（三轮探针实测：CLI / WorkBuddy 的 429 当轮触发 `Stop`，
//! `FinalStop` 是终态补充；`PreToolUse` / `PostToolUse` 之类会随每次工具调用触发，
//! 只增加客户端开销）。hook 脚本把 stdin payload 追加到 `~/.wb-switch/hook-events.jsonl`，
//! 由后端消费（见 `rate_limit_events.rs`）。
//!
//! 三条硬约束（安装会改写用户真实配置，必须守住）：
//! - **幂等**：重复安装不产生重复条目；
//! - **写前备份**：安装前把原文件原样复制到 `~/.wb-switch/hook-backups/`；
//! - **可还原**：卸载时若配置的语义与备份一致（除本工具的条目外没有别的改动），
//!   直接把备份字节写回，做到逐字节还原；用户后来改过别的键时只做结构化移除，不覆盖用户改动。
//!
//! 全部公开入口都通过 [`HookLayout`] 取路径，安装 / 卸载 / 状态的核心实现接收显式 layout，
//! 单测一律注入临时目录，不触碰真实 `~/.codebuddy`、`~/.workbuddy`、`~/.workbuddy-ai`、`~/.wb-switch`。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::config::{atomic_write, home_dir};

/// hook 脚本名（macOS / Linux）。
const SCRIPT_NAME_SH: &str = "hook.sh";
/// hook 脚本名（Windows）。
const SCRIPT_NAME_CMD: &str = "hook.cmd";
/// hook 事件信号文件：脚本 append、后端消费。
pub const EVENTS_FILE_NAME: &str = "hook-events.jsonl";
/// 安装前备份目录（卸载时据此逐字节还原）。
const BACKUP_DIR_NAME: &str = "hook-backups";

/// 注册的事件（最小集）。
const HOOK_EVENTS: [&str; 2] = ["Stop", "FinalStop"];

/// 客户端数据根目录名 → 备份文件名标签（三处配置的稳定标识）。
const TARGETS: [(&str, &str); 3] = [
    (".codebuddy", "codebuddy"),
    (".workbuddy", "workbuddy"),
    (".workbuddy-ai", "workbuddy-ai"),
];

/// 本应用的数据目录名（与 `config::store_dir()` 一致）。
const STORE_DIR_NAME: &str = ".wb-switch";

// ---------------------------------------------------------------------------
// 平台脚本
// ---------------------------------------------------------------------------

/// hook 脚本方言：平台原生。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    /// macOS / Linux：`sh`。
    Sh,
    /// Windows：`cmd` 包一层 PowerShell 读 stdin。
    Cmd,
}

fn script_kind() -> ScriptKind {
    if cfg!(windows) {
        ScriptKind::Cmd
    } else {
        ScriptKind::Sh
    }
}

fn script_name(kind: ScriptKind) -> &'static str {
    match kind {
        ScriptKind::Sh => SCRIPT_NAME_SH,
        ScriptKind::Cmd => SCRIPT_NAME_CMD,
    }
}

/// hook 脚本正文。
///
/// - `Sh`：一次 `cat` 读入 payload、一次 `printf` 追加（尽量单次 write，减少并发追加的
///   行内交错），最后必须回 `{}`——空 stdout 会被客户端当作 hook 失败。
/// - `Cmd`：Windows 没有 `cat`，`findstr` 又有行长上限（429 的 payload 带完整助手消息，
///   可能超限），改用系统自带 PowerShell 读 stdin（UTF-8 保真）。每次事件多一次进程启动，
///   但事件只发生在每轮对话结束时，可接受；PowerShell 不可用时仍回 `{}`，不影响客户端。
fn script_body(kind: ScriptKind) -> &'static str {
    match kind {
        ScriptKind::Sh => {
            "#!/bin/sh\npayload=$(cat)\nprintf '%s\\n' \"$payload\" >> \"$HOME/.wb-switch/hook-events.jsonl\"\nprintf '{}'\n"
        }
        ScriptKind::Cmd => concat!(
            "@echo off\r\n",
            "powershell -NoProfile -ExecutionPolicy Bypass -Command \"$d=[Console]::In.ReadToEnd(); ",
            "if ($d.Trim().Length -gt 0) { Add-Content -LiteralPath (Join-Path $env:USERPROFILE '.wb-switch\\hook-events.jsonl') ",
            "-Value $d.TrimEnd() -Encoding UTF8 }\"\r\n",
            "echo {}\r\n",
        ),
    }
}

/// 单引号包裹 shell 参数（路径可能含空格）。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// hook 脚本的启动命令（写进三处 `settings.json` 的 `command`）。
fn hook_command(script: &Path, kind: ScriptKind) -> String {
    let path = script.to_string_lossy();
    match kind {
        ScriptKind::Sh => format!("sh {}", shell_quote(&path)),
        // bash / cmd 两种解释器下 `cmd /c "…"` 都是合法调用（含空格路径也安全）。
        ScriptKind::Cmd => format!("cmd /c \"{path}\""),
    }
}

// ---------------------------------------------------------------------------
// 目标布局
// ---------------------------------------------------------------------------

/// 一处客户端配置。
struct HookTarget {
    label: &'static str,
    settings: PathBuf,
}

/// 本模块用到的全部路径（显式传入，便于单测注入）。
struct HookLayout {
    script: PathBuf,
    events: PathBuf,
    backups: PathBuf,
    targets: Vec<HookTarget>,
}

impl HookLayout {
    /// 以 `base` 为「用户主目录」推导全部路径。
    fn under(base: &Path) -> Self {
        let store = base.join(STORE_DIR_NAME);
        Self {
            script: store.join(script_name(script_kind())),
            events: store.join(EVENTS_FILE_NAME),
            backups: store.join(BACKUP_DIR_NAME),
            targets: TARGETS
                .iter()
                .map(|(dir, label)| HookTarget {
                    label,
                    settings: base.join(dir).join("settings.json"),
                })
                .collect(),
        }
    }

    fn default_layout() -> Self {
        Self::under(&home_dir())
    }

    fn backup_path(&self, target: &HookTarget) -> PathBuf {
        self.backups.join(format!("{}.settings.json", target.label))
    }

    /// 「安装时该配置不存在」的标记文件。
    fn absent_mark_path(&self, target: &HookTarget) -> PathBuf {
        self.backups
            .join(format!("{}.settings.json.absent", target.label))
    }
}

/// hook 事件信号文件路径（后端消费方使用）。
pub fn events_path() -> PathBuf {
    HookLayout::default_layout().events
}

// ---------------------------------------------------------------------------
// 配置读写
// ---------------------------------------------------------------------------

fn read_settings(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 条目是否属于本工具：嵌套格式里任一 command hook 的命令包含脚本路径。
fn entry_is_ours(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("type").and_then(Value::as_str) == Some("command")
                    && hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| command.contains(marker))
            })
        })
}

/// 配置里是否已注册本工具的 hook（脚本存在性由调用方另外判定）。
fn config_has_marker(root: &Value, marker: &str) -> bool {
    root.get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            HOOK_EVENTS.iter().any(|event| {
                hooks
                    .get(*event)
                    .and_then(Value::as_array)
                    .is_some_and(|entries| entries.iter().any(|e| entry_is_ours(e, marker)))
            })
        })
}

/// 取出可变的事件数组；结构不符（`hooks` 不是对象 / 事件不是数组）时报错而不是覆盖。
fn event_entries<'a>(root: &'a mut Value, event: &str) -> Result<&'a mut Vec<Value>, String> {
    let Some(object) = root.as_object_mut() else {
        return Err("配置根节点不是 JSON 对象".to_string());
    };
    let hooks = object
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err("`hooks` 不是 JSON 对象".to_string());
    };
    let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
    list.as_array_mut()
        .ok_or_else(|| format!("`hooks.{event}` 不是数组"))
}

/// 插入（已存在则更新）本工具在某个事件下的条目：先摘掉旧的同源条目，再追加一条。
fn upsert_event(root: &mut Value, event: &str, command: &str, marker: &str) -> Result<(), String> {
    let entries = event_entries(root, event)?;
    entries.retain(|entry| !entry_is_ours(entry, marker));
    entries.push(json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": command }],
    }));
    Ok(())
}

/// 移除本工具在全部已注册事件下的条目；空数组 / 空 `hooks` 对象一并摘掉。
fn remove_event_entries(root: &mut Value, marker: &str) {
    let Some(object) = root.as_object_mut() else {
        return;
    };
    let Some(hooks) = object.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for event in HOOK_EVENTS {
        let Some(list) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        list.retain(|entry| !entry_is_ours(entry, marker));
        if list.is_empty() {
            hooks.remove(event);
        }
    }
    if hooks.is_empty() {
        object.remove("hooks");
    }
}

fn pretty(root: &Value) -> String {
    serde_json::to_string_pretty(root).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 安装 / 卸载 / 状态
// ---------------------------------------------------------------------------

/// hook 安装状态（含三处配置逐项结果）。
pub fn hook_status() -> Value {
    status_at(&HookLayout::default_layout())
}

/// hook 是否处于「已安装」状态：脚本存在且至少一处配置带 marker。
pub fn is_installed() -> bool {
    hook_status()
        .get("installed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 安装（幂等）：生成脚本 + 在三处配置注册 `Stop` / `FinalStop`，写前备份。
pub fn install_hook() -> Result<Value, String> {
    install_at(&HookLayout::default_layout())
}

/// 卸载：移除三处配置里属于本工具的条目（可逐字节还原），并清理本工具生成的脚本。
pub fn uninstall_hook() -> Result<Value, String> {
    uninstall_at(&HookLayout::default_layout())
}

fn status_at(layout: &HookLayout) -> Value {
    let marker = layout.script.to_string_lossy().to_string();
    let script_exists = layout.script.is_file();
    let mut configured = 0;
    let targets: Vec<Value> = layout
        .targets
        .iter()
        .map(|target| {
            let installed = read_settings(&target.settings)
                .is_some_and(|root| config_has_marker(&root, &marker));
            if installed {
                configured += 1;
            }
            json!({
                "label": target.label,
                "path": target.settings.to_string_lossy(),
                "exists": target.settings.is_file(),
                "installed": installed,
            })
        })
        .collect();
    json!({
        "scriptPath": layout.script.to_string_lossy(),
        "scriptExists": script_exists,
        "eventsPath": layout.events.to_string_lossy(),
        "installed": script_exists && configured > 0,
        "targets": targets,
    })
}

fn install_at(layout: &HookLayout) -> Result<Value, String> {
    write_script(layout)?;
    let command = hook_command(&layout.script, script_kind());
    let marker = layout.script.to_string_lossy().to_string();
    let mut errors = Vec::new();
    for target in &layout.targets {
        if let Err(error) = install_target(layout, target, &command, &marker) {
            errors.push(format!("{}：{error}", target.settings.display()));
        }
    }
    if errors.is_empty() {
        Ok(status_at(layout))
    } else {
        Err(errors.join("；"))
    }
}

fn write_script(layout: &HookLayout) -> Result<(), String> {
    if let Some(parent) = layout.script.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
    }
    let body = script_body(script_kind());
    // 内容一致就不写：避免重复安装改动 mtime，也避免与手动编辑过的脚本互相覆盖。
    if std::fs::read_to_string(&layout.script).ok().as_deref() == Some(body) {
        return Ok(());
    }
    std::fs::write(&layout.script, body)
        .map_err(|error| format!("写入 {} 失败：{error}", layout.script.display()))
}

fn install_target(
    layout: &HookLayout,
    target: &HookTarget,
    command: &str,
    marker: &str,
) -> Result<(), String> {
    let original = std::fs::read_to_string(&target.settings).ok();
    let current = match &original {
        Some(text) => Some(
            serde_json::from_str::<Value>(text)
                .map_err(|_| "不是合法 JSON，已跳过（未做任何改动）".to_string())?,
        ),
        None => None,
    };
    // 备份基线：本次安装「除了本工具条目之外」的内容。
    let baseline = match &current {
        Some(root) => {
            let mut clean = root.clone();
            remove_event_entries(&mut clean, marker);
            clean
        }
        None => json!({}),
    };
    refresh_backup(layout, target, original.as_deref(), &baseline, marker)?;

    let mut root = current.unwrap_or_else(|| json!({}));
    for event in HOOK_EVENTS {
        upsert_event(&mut root, event, command, marker)?;
    }
    let content = pretty(&root);
    if original.as_deref() == Some(content.as_str()) {
        return Ok(());
    }
    if let Some(parent) = target.settings.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
    }
    atomic_write(&target.settings, &content).map_err(|error| format!("写入失败：{error}"))?;
    Ok(())
}

/// 维护备份基线：已有且语义一致就保留；否则记录「当前减去本工具条目」的形态。
///
/// 首次安装（原文件没有我们的 marker）时基线取**原文件原始字节**，卸载可直接逐字节还原。
fn refresh_backup(
    layout: &HookLayout,
    target: &HookTarget,
    original: Option<&str>,
    baseline: &Value,
    marker: &str,
) -> Result<(), String> {
    let backup = layout.backup_path(target);
    if let Some(parent) = backup.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
    }
    let absent = layout.absent_mark_path(target);
    match original {
        None => {
            if !backup.exists() && !absent.exists() {
                std::fs::write(&absent, "")
                    .map_err(|error| format!("写入备份标记失败：{error}"))?;
            }
        }
        Some(text) => {
            let raw_is_clean = !text.contains(marker);
            if let Ok(existing) = std::fs::read_to_string(&backup) {
                let same = serde_json::from_str::<Value>(&existing).ok().as_ref() == Some(baseline);
                if same {
                    return Ok(());
                }
            }
            let content = if raw_is_clean {
                text.to_string()
            } else {
                pretty(baseline)
            };
            std::fs::write(&backup, content).map_err(|error| format!("写入备份失败：{error}"))?;
        }
    }
    Ok(())
}

fn uninstall_at(layout: &HookLayout) -> Result<Value, String> {
    let marker = layout.script.to_string_lossy().to_string();
    let mut errors = Vec::new();
    for target in &layout.targets {
        if let Err(error) = uninstall_target(layout, target, &marker) {
            errors.push(format!("{}：{error}", target.settings.display()));
        }
    }
    // 脚本只在内容仍是本工具生成的那一份时删除；用户改过就保留，不做猜测。
    if std::fs::read_to_string(&layout.script).ok().as_deref() == Some(script_body(script_kind())) {
        let _ = std::fs::remove_file(&layout.script);
    }
    if errors.is_empty() {
        Ok(status_at(layout))
    } else {
        Err(errors.join("；"))
    }
}

fn uninstall_target(layout: &HookLayout, target: &HookTarget, marker: &str) -> Result<(), String> {
    let Some(original) = std::fs::read_to_string(&target.settings).ok() else {
        return Ok(());
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&original) else {
        // 损坏的配置不动：宁可留着 marker，也不覆盖用户（或客户端）写坏的内容。
        return Ok(());
    };
    remove_event_entries(&mut root, marker);
    let cleaned = pretty(&root);

    if let Ok(bytes) = std::fs::read_to_string(layout.backup_path(target)) {
        if serde_json::from_str::<Value>(&bytes).ok().as_ref() == Some(&root) {
            // 语义与备份一致 = 安装之后没有别的改动 → 直接写回原始字节。
            if bytes != original {
                atomic_write(&target.settings, &bytes)
                    .map_err(|error| format!("还原失败：{error}"))?;
            }
            return Ok(());
        }
    } else if layout.absent_mark_path(target).exists() && root == json!({}) {
        // 安装前该配置不存在，卸载后内容为空 → 恢复「不存在」。
        std::fs::remove_file(&target.settings)
            .map_err(|error| format!("删除空配置失败：{error}"))?;
        return Ok(());
    }

    if cleaned != original {
        atomic_write(&target.settings, &cleaned).map_err(|error| format!("写入失败：{error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> HookLayout {
        let base =
            std::env::temp_dir().join(format!("wb-switch-hook-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).expect("临时主目录");
        HookLayout::under(&base)
    }

    fn target<'a>(layout: &'a HookLayout, label: &str) -> &'a HookTarget {
        layout
            .targets
            .iter()
            .find(|target| target.label == label)
            .expect("目标配置必须存在")
    }

    fn write_settings(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("配置目录");
        std::fs::write(path, content).expect("写入配置");
    }

    #[test]
    fn script_body_appends_payload_and_always_returns_empty_object() {
        let sh = script_body(ScriptKind::Sh);
        assert!(sh.contains("hook-events.jsonl"), "{sh}");
        assert!(sh.trim_end().ends_with("printf '{}'"), "{sh}");

        let cmd = script_body(ScriptKind::Cmd);
        assert!(cmd.contains("hook-events.jsonl"), "{cmd}");
        assert!(cmd.trim_end().ends_with("echo {}"), "{cmd}");
    }

    #[test]
    fn hook_command_quotes_the_script_path() {
        let path = Path::new("/Users/a b/.wb-switch/hook.sh");
        assert_eq!(
            hook_command(path, ScriptKind::Sh),
            "sh '/Users/a b/.wb-switch/hook.sh'"
        );
        assert_eq!(
            hook_command(path, ScriptKind::Cmd),
            "cmd /c \"/Users/a b/.wb-switch/hook.sh\""
        );
    }

    #[test]
    fn install_is_idempotent_and_preserves_third_party_hooks() {
        let layout = layout();
        let codebuddy = target(&layout, "codebuddy");
        write_settings(
            &codebuddy.settings,
            &serde_json::to_string_pretty(&json!({
                "model": "deepseek-v4.1-flash",
                "hooks": {
                    "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "echo user" }] }],
                    "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo tool" }] }],
                },
            }))
            .expect("序列化"),
        );

        for _ in 0..2 {
            install_at(&layout).expect("安装必须成功");
        }

        let root = read_settings(&codebuddy.settings).expect("配置可解析");
        let stop = root["hooks"]["Stop"].as_array().expect("Stop 数组");
        assert_eq!(stop.len(), 2, "只应追加一条我们的条目：{stop:?}");
        assert_eq!(stop[0]["hooks"][0]["command"], "echo user", "用户条目保留");
        assert!(stop[1]["hooks"][0]["command"]
            .as_str()
            .expect("命令")
            .contains("hook.sh"));
        assert_eq!(
            root["hooks"]["FinalStop"]
                .as_array()
                .expect("FinalStop 数组")
                .len(),
            1
        );
        assert_eq!(
            root["hooks"]["PreToolUse"]
                .as_array()
                .expect("PreToolUse 数组")
                .len(),
            1,
            "其它事件不得被动"
        );
        assert_eq!(root["model"], "deepseek-v4.1-flash");
        // 未注册的事件一个都不能多。
        assert_eq!(
            root["hooks"].as_object().expect("hooks 对象").len(),
            3,
            "只有 Stop / FinalStop 是本工具新增的"
        );
    }

    #[test]
    fn uninstall_restores_the_settings_file_byte_for_byte() {
        let layout = layout();
        for label in ["codebuddy", "workbuddy", "workbuddy-ai"] {
            let target = target(&layout, label);
            // 故意用非标准格式（紧凑 + 无缩进）验证「逐字节还原」，而不是「重排后相等」。
            write_settings(
                &target.settings,
                r#"{"language":"简体中文","hooks":{"Stop":[{"matcher":"x","hooks":[{"type":"command","command":"echo keep"}]}]},"model":"hy3"}"#,
            );
        }

        install_at(&layout).expect("安装");
        let installed = read_settings(&target(&layout, "codebuddy").settings).expect("已安装");
        assert!(config_has_marker(
            &installed,
            &layout.script.to_string_lossy()
        ));

        uninstall_at(&layout).expect("卸载");
        for label in ["codebuddy", "workbuddy", "workbuddy-ai"] {
            let target = target(&layout, label);
            assert_eq!(
                std::fs::read_to_string(&target.settings).expect("配置存在"),
                r#"{"language":"简体中文","hooks":{"Stop":[{"matcher":"x","hooks":[{"type":"command","command":"echo keep"}]}]},"model":"hy3"}"#,
                "{label} 必须逐字节还原"
            );
        }
        assert!(!layout.script.is_file(), "本工具生成的脚本在卸载后应清理");
    }

    #[test]
    fn uninstall_keeps_changes_the_user_made_after_install() {
        let layout = layout();
        let codebuddy = target(&layout, "codebuddy");
        write_settings(&codebuddy.settings, "{}");
        install_at(&layout).expect("安装");

        // 安装之后用户（或客户端）改了别的键。
        let mut root = read_settings(&codebuddy.settings).expect("配置");
        root["statusLine"] = json!({ "type": "command" });
        write_settings(&codebuddy.settings, &pretty(&root));

        uninstall_at(&layout).expect("卸载");
        let root = read_settings(&codebuddy.settings).expect("配置");
        assert_eq!(root["statusLine"]["type"], "command", "用户的改动必须保留");
        assert!(
            !config_has_marker(&root, &layout.script.to_string_lossy()),
            "本工具条目必须移除"
        );
        assert!(root.get("hooks").is_none(), "空 hooks 键一并摘掉");
    }

    #[test]
    fn install_creates_missing_settings_and_uninstall_removes_them_again() {
        let layout = layout();
        let workbuddy = target(&layout, "workbuddy");
        assert!(!workbuddy.settings.exists());

        install_at(&layout).expect("安装");
        let root = read_settings(&workbuddy.settings).expect("新配置可解析");
        assert!(config_has_marker(&root, &layout.script.to_string_lossy()));

        uninstall_at(&layout).expect("卸载");
        assert!(
            !workbuddy.settings.exists(),
            "安装前不存在 → 卸载后也不存在"
        );
    }

    #[test]
    fn malformed_settings_are_left_untouched() {
        let layout = layout();
        let workbuddy = target(&layout, "workbuddy");
        write_settings(&workbuddy.settings, "{ not json");
        let codebuddy = target(&layout, "codebuddy");
        write_settings(&codebuddy.settings, "{}");

        let error = install_at(&layout).expect_err("非法 JSON 必须报错");
        assert!(error.contains("workbuddy"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&workbuddy.settings).expect("原文件仍在"),
            "{ not json",
            "损坏的配置不得被覆盖"
        );
        // 其它目标照常安装（单个失败不阻断）。
        assert!(config_has_marker(
            &read_settings(&codebuddy.settings).expect("配置"),
            &layout.script.to_string_lossy()
        ));
    }

    #[test]
    fn malformed_hooks_shape_is_reported_without_overwriting() {
        let layout = layout();
        let codebuddy = target(&layout, "codebuddy");
        write_settings(
            &codebuddy.settings,
            &json!({ "hooks": ["not-an-object"] }).to_string(),
        );
        let error = install_at(&layout).expect_err("`hooks` 不是对象必须报错");
        assert!(error.contains("hooks"), "{error}");
        assert_eq!(
            read_settings(&codebuddy.settings).expect("配置")["hooks"][0],
            "not-an-object"
        );
    }

    #[test]
    fn status_tracks_the_marker_and_the_script() {
        let layout = layout();
        let status = status_at(&layout);
        assert_eq!(status["installed"], json!(false));
        assert_eq!(status["scriptExists"], json!(false));
        assert_eq!(status["targets"].as_array().expect("targets").len(), 3);

        install_at(&layout).expect("安装");
        let status = status_at(&layout);
        assert_eq!(status["installed"], json!(true));
        assert_eq!(status["scriptExists"], json!(true));
        for target in status["targets"].as_array().expect("targets") {
            assert_eq!(target["installed"], json!(true), "{target}");
        }

        // 配置里手删 marker（模拟客户端升级冲掉了注册）→ 不再视为已安装。
        let codebuddy = target(&layout, "codebuddy");
        std::fs::remove_file(&codebuddy.settings).expect("删除配置");
        assert_eq!(
            status_at(&layout)["installed"],
            json!(true),
            "还有两处配置在"
        );
        for label in ["workbuddy", "workbuddy-ai"] {
            std::fs::remove_file(&target(&layout, label).settings).expect("删除配置");
        }
        assert_eq!(status_at(&layout)["installed"], json!(false));
    }

    #[test]
    fn events_path_lives_in_the_store_directory() {
        assert!(events_path().ends_with(EVENTS_FILE_NAME));
        assert!(events_path().to_string_lossy().contains(STORE_DIR_NAME));
    }
}
