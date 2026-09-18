//! 会话列表与按需复制（路径 B：生成新 id，云端可正常同步）。
//!
//! 对照 server.py `current_user_uid` / `list_sessions_for_user` /
//! `_find_project_jsonl` / `copy_session_to_user` / `_register_edge_sync_mapping` /
//! `copy_sessions_for_switch` / `backup_workbuddy_db` / `workbuddy_db_path`。
//!
//! WorkBuddy 5.x 数据三件套（缺一不可）：
//!   1) 正文：`~/.workbuddy/projects/{workspace}/{cid}.jsonl`（JSONL 含 sessionId 字段）
//!   2) 元数据：`~/.workbuddy/workbuddy.db` sessions 表（id = conversation id = UUID）
//!   3) 云端映射：`~/.workbuddy/edge-sync-mapping-v2.db` edge_sync_mapping
//!      （session_id=conversation_id，msg_channel=convmsg:{uid} 决定云端归属）
//!
//! 复制收口（design §4）：所有复制入口统一走 [`copy_sessions_for_switch`]，在同一把
//! 档位操作锁内先恢复未完成操作、再查询关联组；同一逻辑会话只保留一个有效副本，
//! 目标 UUID 在任何副本写入前持久化，任一阶段失败都不报告完整成功，恢复复用同一
//! UUID 且不产生第二个副本。

use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::modules::account;
use crate::modules::auth_file;
use crate::modules::config::{atomic_write, now_ms, now_secs, store_dir, utc_iso};
use crate::modules::process;
use crate::modules::session_link::{
    self, ContentSnapshot, ContentState, LinkGroup, LinkMember, MemberState, NormalizedContent,
    OpPhase, Operation, OperationMember, RecoveryIssue, RecoveryReport, StoreState,
    NORMALIZATION_VERSION, OPERATION_VERSION,
};
use crate::modules::variant::WbVariant;

/// 会话操作涉及的路径集合：工具存储根（`~/.wb-switch`）与档位数据根。
///
/// 生产入口用 [`SessionPaths::for_variant`]；单测注入临时目录，
/// 绝不触碰真实 `~/.wb-switch` 或 WorkBuddy 数据目录。
#[derive(Clone, Debug)]
pub struct SessionPaths {
    /// 工具存储根：关联表、基线、操作日志、锁与备份都在这里。
    pub store_root: PathBuf,
    /// 档位数据根：`projects/`、`workbuddy.db`、`edge-sync-mapping-*.db`。
    pub data_root: PathBuf,
    /// 该档位的官方登录态文件（来源账号 uid 的判据）。
    pub auth_file: PathBuf,
}

impl SessionPaths {
    pub fn for_variant(variant: WbVariant) -> Self {
        Self {
            store_root: store_dir(),
            data_root: variant.data_root(),
            auth_file: variant.auth_file_path(),
        }
    }

    pub fn workbuddy_db(&self) -> PathBuf {
        self.data_root.join("workbuddy.db")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.data_root.join("projects")
    }

    pub fn edge_sync_db(&self, variant: WbVariant) -> PathBuf {
        self.data_root.join(edge_sync_db_name(variant))
    }

    pub fn backup_root(&self) -> PathBuf {
        self.store_root.join("backups")
    }

    pub fn session_links_file(&self) -> PathBuf {
        self.store_root.join("session_links.json")
    }

    pub fn session_links_dir(&self) -> PathBuf {
        self.store_root.join("session-links")
    }

    pub fn baselines_dir(&self) -> PathBuf {
        self.session_links_dir().join("baselines")
    }

    pub fn operations_dir(&self) -> PathBuf {
        self.session_links_dir().join("operations")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.store_root.join("locks")
    }

    pub fn variant_ops_lock_file(&self, variant: WbVariant) -> PathBuf {
        self.locks_dir()
            .join(format!("session-ops-{}.lock", variant.as_str()))
    }

    pub fn link_store_lock_file(&self) -> PathBuf {
        self.locks_dir().join("session-links.lock")
    }
}

/// 打开数据库并设置 busy_timeout（对照 Python `sqlite3.connect(timeout=5)`）。
pub(crate) fn open_db(path: &Path, read_only: bool) -> Option<Connection> {
    let conn = if read_only {
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?
    } else {
        Connection::open(path).ok()?
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    Some(conn)
}

/// 客户端数据根下的会话数据库（按档位取根）。
pub fn workbuddy_db_path(variant: WbVariant) -> PathBuf {
    variant.data_root().join("workbuddy.db")
}

/// 云端映射库文件名：国内版历史为 v2；国际版实测为 v4（两档位不同构）。
fn edge_sync_db_name(variant: WbVariant) -> &'static str {
    match variant {
        WbVariant::Cn => "edge-sync-mapping-v2.db",
        WbVariant::Ai => "edge-sync-mapping-v4.db",
    }
}

/// 会话复制能力探测：数据根同时具备 `projects/` 目录与 `workbuddy.db` 的
/// `sessions` 表才算可用（design D6）。
///
/// 为什么必须探测而不是按档位写死：国际版数据根与国内版**不同构**——本机实测
/// 国际版数据根下没有 `projects/`、edge-sync 为 v4。若直接套用国内版假设，
/// 会写出「有 db 记录但没有正文」的半成品会话。
///
/// 纯函数，接受根路径参数以便用临时目录做单元测试。
pub fn session_copy_supported_at(root: &Path) -> bool {
    if !root.join("projects").is_dir() {
        return false;
    }
    let db = root.join("workbuddy.db");
    if !db.is_file() {
        return false;
    }
    let Some(conn) = open_db(&db, true) else {
        return false;
    };
    table_exists(&conn, "sessions")
}

/// 档位不支持会话复制时的统一错误文案。
pub const SESSION_COPY_UNSUPPORTED: &str = "该档位暂不支持会话复制";

/// WorkBuddy 正在运行时的统一错误文案：会话写入必须在 App 停止写入之后。
pub const SESSION_COPY_APP_RUNNING: &str =
    "WorkBuddy 正在运行，已阻止直接写入会话数据；请先退出 WorkBuddy 后重试";

/// 操作日志无法解析时的原因前缀（恢复与复制共用，避免漏报后写出第二个副本）。
const UNPARSEABLE_OPERATION_REASON: &str = "操作记录无法解析";

/// 当前认证账号的 uid（该档位认证文件的 account.uid）。
pub fn current_user_uid(variant: WbVariant) -> Option<String> {
    current_user_uid_at(&variant.auth_file_path())
}

/// 从指定登录态文件读取 uid（单测注入临时文件用）。
pub fn current_user_uid_at(auth_file: &Path) -> Option<String> {
    let auth = auth_file::read_auth_file_at(auth_file)?;
    auth.get("account")
        .and_then(|a| a.get("uid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub(crate) fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        == 1
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let names: Vec<String> = iter.flatten().collect();
    names.iter().any(|name| name == column)
}

fn nonempty_text(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// WorkBuddy 侧栏展示名：优先 custom_title（用户改名 / 定时任务名），否则 title。
fn session_display_title(title: Option<String>, custom_title: Option<String>) -> String {
    nonempty_text(custom_title)
        .or_else(|| nonempty_text(title))
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// Claw 是账号绑定的 IM 渠道工作区，复制会话行不够，目标账号也用不了。
fn is_claw_workspace(cwd: &str) -> bool {
    cwd.trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("claw"))
}

/// 列出某账号未删除的会话（workbuddy.db sessions 表，db 为准）。
///
/// `title` 为 WorkBuddy 侧栏同款展示名；`isPlayground` 对应侧栏「任务」，
/// 其余按 `cwd` 最后一段归入「空间」。
pub fn list_sessions_for_user(variant: WbVariant, uid: &str) -> Value {
    list_sessions_for_user_at(&SessionPaths::for_variant(variant), uid)
}

fn list_sessions_for_user_at(paths: &SessionPaths, uid: &str) -> Value {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return json!([]);
    }
    let Some(conn) = open_db(&db, true) else {
        return json!([]);
    };
    if !table_exists(&conn, "sessions") {
        return json!([]);
    }
    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let has_playground = column_exists(&conn, "sessions", "is_playground");
    let sql = match (has_custom, has_playground) {
        (true, true) => {
            "SELECT id, cwd, title, custom_title, updated_at, is_playground FROM sessions \
             WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC"
        }
        (true, false) => {
            "SELECT id, cwd, title, custom_title, updated_at, 0 FROM sessions \
             WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC"
        }
        (false, true) => {
            "SELECT id, cwd, title, NULL, updated_at, is_playground FROM sessions \
             WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC"
        }
        (false, false) => {
            "SELECT id, cwd, title, NULL, updated_at, 0 FROM sessions \
             WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC"
        }
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return json!([]),
    };
    let rows = stmt.query_map([uid], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    });

    let mut sessions: Vec<Value> = Vec::new();
    if let Ok(iter) = rows {
        for r in iter.flatten() {
            let (cid, cwd, title, custom_title, updated_at, is_playground) = r;
            let cid = cid.unwrap_or_default();
            let cwd = cwd.unwrap_or_default();
            if is_claw_workspace(&cwd) {
                continue;
            }
            sessions.push(json!({
                "id": cid,
                "title": session_display_title(title, custom_title),
                "cwd": cwd,
                "updatedAt": updated_at.unwrap_or(0),
                "hasHistory": find_project_jsonl(paths, &cid).is_some(),
                "isPlayground": is_playground.unwrap_or(0) != 0,
            }));
        }
    }
    json!(sessions)
}

/// 在 `{档位数据根}/projects/{workspace}/{cid}.jsonl` 定位会话正文。
fn find_project_jsonl(paths: &SessionPaths, cid: &str) -> Option<PathBuf> {
    let projects = paths.projects_dir();
    if !projects.is_dir() {
        return None;
    }
    let direct = projects.join(format!("{cid}.jsonl"));
    if direct.is_file() {
        return Some(direct);
    }
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let p = entry.path().join(format!("{cid}.jsonl"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 备份 workbuddy.db（含 -wal/-shm），返回主库备份路径。
///
/// 任何一步失败都返回 Err——不能沿用「忽略 copy 错误后仍宣称备份成功」的旧行为，
/// 备份不可信时后续数据库写入必须先停下来（design §1）。
fn backup_workbuddy_db(paths: &SessionPaths, backup_root: &Path) -> Result<PathBuf, String> {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return Err("会话数据库不存在，未复制".to_string());
    }
    std::fs::create_dir_all(backup_root).map_err(|error| format!("备份目录创建失败：{error}"))?;
    for suffix in ["", "-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{}", db.to_string_lossy(), suffix));
        if !src.is_file() {
            continue;
        }
        let dest = backup_root.join(format!("workbuddy.db{suffix}"));
        std::fs::copy(&src, &dest).map_err(|error| format!("备份 {suffix} 失败：{error}"))?;
        let (src_len, dest_len) = (
            std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0),
            std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
        );
        if src_len != dest_len {
            return Err(format!("备份 {suffix} 校验失败：大小不一致，未复制"));
        }
    }
    Ok(backup_root.join("workbuddy.db"))
}

/// 会话数据库复制前的一次性备份目录（按档位分目录，两档位不互相覆盖）。
fn session_backup_root(paths: &SessionPaths, variant: WbVariant) -> PathBuf {
    paths
        .backup_root()
        .join("sessions")
        .join(variant.as_str())
        .join(utc_iso())
}

/// 数据库插入结果：`No*` 与 `SourceRowMissing` 都不允许被当成成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbCopyOutcome {
    Inserted,
    SourceRowMissing,
    NoSessionsTable,
    NoDb,
}

/// 会话行归属（未删除时）。
fn session_row_owner(paths: &SessionPaths, cid: &str) -> Option<String> {
    let db = paths.workbuddy_db();
    let conn = open_db(&db, true)?;
    conn.query_row(
        "SELECT user_id FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
        [cid],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// 在 workbuddy.db 中把源会话行复制为新 id（动态列，覆盖 id/user_id/时间戳）。
///
/// 与旧实现不同：db/表/源行缺失都显式返回，不再静默 Ok。
fn insert_session_copy(
    paths: &SessionPaths,
    new_cid: &str,
    cid: &str,
    source_uid: &str,
    target_uid: &str,
) -> Result<DbCopyOutcome, String> {
    let db_path = paths.workbuddy_db();
    if !db_path.is_file() {
        return Ok(DbCopyOutcome::NoDb);
    }
    let Some(conn) = open_db(&db_path, false) else {
        return Err("会话数据库无法打开".to_string());
    };
    if !table_exists(&conn, "sessions") {
        return Ok(DbCopyOutcome::NoSessionsTable);
    }
    let mut src_stmt = conn
        .prepare("SELECT * FROM sessions WHERE id = ?1 AND user_id = ?2")
        .map_err(|e| e.to_string())?;
    let cols: Vec<String> = src_stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows = src_stmt
        .query(rusqlite::params![cid, source_uid])
        .map_err(|e| e.to_string())?;
    let Some(row) = rows.next().map_err(|e| e.to_string())? else {
        return Ok(DbCopyOutcome::SourceRowMissing);
    };
    let mut vals: Vec<rusqlite::types::Value> = Vec::with_capacity(cols.len());
    for (i, col) in cols.iter().enumerate() {
        let v = row
            .get::<_, rusqlite::types::Value>(i)
            .unwrap_or(rusqlite::types::Value::Null);
        if col == "cwd" {
            if let rusqlite::types::Value::Text(ref path) = v {
                if is_claw_workspace(path) {
                    return Err("Claw 工作区绑定当前账号渠道，不支持复制".to_string());
                }
            }
        }
        match col.as_str() {
            "id" => vals.push(rusqlite::types::Value::Text(new_cid.to_string())),
            "user_id" => vals.push(rusqlite::types::Value::Text(target_uid.to_string())),
            "created_at" | "updated_at" => vals.push(rusqlite::types::Value::Integer(now_ms())),
            "deleted_at" => vals.push(rusqlite::types::Value::Null),
            _ => vals.push(v),
        }
    }
    drop(rows);
    drop(src_stmt);

    let placeholders = cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let colnames = cols.join(", ");
    // 用 INSERT 而不是 INSERT OR REPLACE：新 UUID 撞库时宁可失败，也不能悄悄覆盖既有会话。
    let sql = format!("INSERT INTO sessions ({colnames}) VALUES ({placeholders})");
    let params: Vec<&rusqlite::types::Value> = vals.iter().collect();
    conn.execute(&sql, rusqlite::params_from_iter(params))
        .map_err(|e| format!("会话行写入失败：{e}"))?;
    Ok(DbCopyOutcome::Inserted)
}

/// 写后校验：目标行必须存在、归属目标账号且未删除。
fn verify_session_row(paths: &SessionPaths, new_cid: &str, target_uid: &str) -> Result<(), String> {
    match session_row_owner(paths, new_cid) {
        Some(owner) if owner == target_uid => Ok(()),
        Some(owner) => Err(format!(
            "会话行归属校验失败：期望 {target_uid}，实际 {owner}"
        )),
        None => Err("会话行写入后不可见，未报告成功".to_string()),
    }
}

/// 云端映射登记结果：失败必须上报，不能静默降级成成功。
#[derive(Debug, Clone)]
enum MappingOutcome {
    Registered,
    Unavailable(String),
}

/// 把新会话注册进 edge_sync_mapping（云端归属关键）。沿用既有登记方式，不扩大作用。
fn register_edge_sync_mapping(
    paths: &SessionPaths,
    variant: WbVariant,
    new_cid: &str,
    target_uid: &str,
) -> MappingOutcome {
    let db_path = paths.edge_sync_db(variant);
    if !db_path.is_file() {
        return MappingOutcome::Unavailable(format!(
            "云端映射库 {} 不存在",
            db_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
    }
    let Some(conn) = open_db(&db_path, false) else {
        return MappingOutcome::Unavailable("云端映射库无法打开".to_string());
    };
    if !table_exists(&conn, "edge_sync_mapping") {
        return MappingOutcome::Unavailable("云端映射库缺少 edge_sync_mapping 表".to_string());
    }
    let result = conn.execute(
        "INSERT OR REPLACE INTO edge_sync_mapping \
         (session_id, conversation_id, msg_channel, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            new_cid,
            new_cid,
            format!("convmsg:{target_uid}"),
            now_secs()
        ],
    );
    match result {
        Ok(_) => MappingOutcome::Registered,
        Err(error) => MappingOutcome::Unavailable(format!("云端映射登记失败：{error}")),
    }
}

/// 目标会话是否已按预期登记在云端映射表（恢复跳过重放时只核验，不 INSERT）。
fn mapping_row_matches(
    paths: &SessionPaths,
    variant: WbVariant,
    session_id: &str,
    target_uid: &str,
) -> bool {
    let db_path = paths.edge_sync_db(variant);
    if !db_path.is_file() {
        return false;
    }
    let Some(conn) = open_db(&db_path, true) else {
        return false;
    };
    if !table_exists(&conn, "edge_sync_mapping") {
        return false;
    }
    let expected = format!("convmsg:{target_uid}");
    conn.query_row(
        "SELECT msg_channel FROM edge_sync_mapping WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .is_some_and(|channel| channel == expected)
}

/// 把勾选的会话复制到目标账号（路径 B）。返回复制报告。
///
/// 档位以**目标账号**自身为准：数据根、数据库、备份目录、认证文件都取该档位。
/// 国际版能力不满足时直接返回明确错误，绝不写半成品（design D6）。
/// App 正在运行时拒绝写入：独立复制 API 与桌面端共用同一条生命周期保护。
pub fn copy_sessions_for_switch(
    target_acc: &Value,
    session_ids: &[String],
) -> Result<Value, String> {
    let variant = account::variant_of(target_acc);
    let paths = SessionPaths::for_variant(variant);
    copy_sessions_for_switch_at(
        &paths,
        variant,
        target_acc,
        session_ids,
        process::is_workbuddy_running,
    )
}

/// 可注入路径与「App 是否运行」探针的复制入口（单测注入临时目录与假探针，不触碰
/// 真实路径、不探测真实进程）。
///
/// App 运行检查做两次：拿档位操作锁之前先快速拒绝；拿锁之后再复查——锁前到拿锁之间
/// App 可能被启动，只有锁后复查才能保证会话写入发生在 App 停止写入之后（design §4.1）。
fn copy_sessions_for_switch_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_acc: &Value,
    session_ids: &[String],
    is_app_running: impl Fn(WbVariant) -> bool,
) -> Result<Value, String> {
    if is_app_running(variant) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    // 探测只对国际版生效（design D6 针对的是国际版数据根不同构）。国内版数据根与
    // 改造前同构，保留改造前的路径与返回结构，不让国内版看到「暂不支持」类新文案。
    if variant == WbVariant::Ai && !session_copy_supported_at(&paths.data_root) {
        return Err(format!(
            "{SESSION_COPY_UNSUPPORTED}（档位 {}）",
            variant.as_str()
        ));
    }
    let target_uid = target_acc
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法复制会话".to_string());
    }
    let source_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法确定来源账号".to_string())?;
    if source_uid == target_uid {
        return Err("当前账号与目标账号相同，无需复制会话".to_string());
    }

    // 档位操作锁覆盖「恢复 → 查询关联 → 写副本 → 提交关联」全过程，
    // 并发请求与中断重试因此不会各自写出第二个副本。
    let _ops_lock = session_link::try_acquire_variant_ops_lock(paths, variant)?;
    // 复查：锁前未运行、拿锁后 App 已被启动的情况在这里被拦住，不写入任何产物。
    if is_app_running(variant) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    let recovery = recover_pending_session_operations_at(paths, variant);
    let pending = session_link::pending_operations(paths, variant);

    let context = CopyContext {
        paths,
        variant,
        source_uid: &source_uid,
        source_account_id: account_id_for_uid(paths, &source_uid),
        target_uid: &target_uid,
        target_account_id: nonempty_text(
            target_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
        pending: &pending,
    };

    let mut copied: Vec<Value> = Vec::new();
    let mut already_linked: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    // 解析失败的操作日志无法对应到具体会话：继续复制会绕过 pending 去重，
    // 可能对同一请求再写出第二个副本。
    if let Some(issue) = recovery
        .needs_recovery
        .iter()
        .find(|issue| !issue.retryable && issue.reason.contains(UNPARSEABLE_OPERATION_REASON))
    {
        for cid in session_ids {
            errors.push(json!({"id": cid, "error": issue.reason.clone()}));
        }
    } else {
        for cid in session_ids {
            match copy_one_session(&context, cid) {
                Ok(CopyOutcome::Copied {
                    new_id,
                    group_id,
                    backup,
                }) => copied.push(json!({
                    "id": cid,
                    "newId": new_id,
                    "groupId": group_id,
                    "backup": backup,
                })),
                Ok(CopyOutcome::AlreadyLinked {
                    session_id,
                    group_id,
                }) => already_linked.push(json!({
                    "id": cid,
                    "sessionId": session_id,
                    "groupId": group_id,
                })),
                Err(error) => errors.push(json!({"id": cid, "error": error})),
            }
        }
    }

    // 本次请求之后仍存在未完成操作（含本次刚留下的）→ 必须提示恢复需求。
    let unfinished_after = session_link::pending_operations(paths, variant);
    let unusable = !recovery.is_clean() || !unfinished_after.is_empty();
    let mut report = json!({
        "sourceUid": source_uid,
        "targetUid": target_uid,
        "copied": copied,
        "alreadyLinked": already_linked,
    });
    if !errors.is_empty() {
        report["errors"] = json!(errors);
    }
    if unusable {
        report["needsRecovery"] = json!(true);
    }
    Ok(report)
}

/// 复制上下文中不变的输入（避免逐会话重复解析）。
struct CopyContext<'a> {
    paths: &'a SessionPaths,
    variant: WbVariant,
    source_uid: &'a str,
    source_account_id: Option<String>,
    target_uid: &'a str,
    target_account_id: Option<String>,
    pending: &'a [Operation],
}

/// 单个会话的一次复制结果。
enum CopyOutcome {
    Copied {
        new_id: String,
        group_id: String,
        backup: String,
    },
    AlreadyLinked {
        session_id: String,
        group_id: String,
    },
}

/// 目标解析：组内目标账号是否已有可复用的有效副本。
struct TargetResolution {
    group_id: Option<String>,
    existing_link: Option<String>,
}

/// 目标账号在关联组内的 active 成员是否真实有效：正文可验证 + 会话行归属正确。
fn member_is_valid(paths: &SessionPaths, member: &LinkMember) -> bool {
    let Some(body) = find_project_jsonl(paths, &member.session_id) else {
        return false;
    };
    match session_link::read_content_snapshot(&body, &member.session_id) {
        ContentState::Ready(_) => {}
        ContentState::Missing | ContentState::Unavailable(_) => return false,
    }
    session_row_owner(paths, &member.session_id).is_some_and(|owner| owner == member.uid)
}

/// 解析（variant, 来源会话）所属组，以及目标账号是否已有有效副本。
fn resolve_target(context: &CopyContext, cid: &str) -> Result<TargetResolution, String> {
    let store = match session_link::load_store(context.paths) {
        StoreState::Missing => {
            return Ok(TargetResolution {
                group_id: None,
                existing_link: None,
            })
        }
        StoreState::Ready(store) => store,
        StoreState::Unavailable(reason) => {
            return Err(format!("{reason}；已阻止复制"));
        }
    };
    let Some(group) =
        session_link::find_group_for_identity(&store, context.variant, context.source_uid, cid)
    else {
        return Ok(TargetResolution {
            group_id: None,
            existing_link: None,
        });
    };
    let group_id = Some(group.id.clone());
    let Some(member) = session_link::active_member_for(group, context.target_uid) else {
        return Ok(TargetResolution {
            group_id,
            existing_link: None,
        });
    };
    if member_is_valid(context.paths, member) {
        return Ok(TargetResolution {
            group_id,
            existing_link: Some(member.session_id.clone()),
        });
    }
    // 失效成员保留记录、不自动复活；本次会重建一个新成员。
    Ok(TargetResolution {
        group_id,
        existing_link: None,
    })
}

/// 写入副本正文并做写后校验（复用同一次源快照，避免 TOCTOU）。
fn write_copy_body(
    snapshot: &ContentSnapshot,
    source_path: &Path,
    cid: &str,
    new_cid: &str,
) -> Result<PathBuf, String> {
    let dest = source_path.with_file_name(format!("{new_cid}.jsonl"));
    if dest.exists() {
        return Err("目标正文已存在同名文件，已停止复制".to_string());
    }
    let text = snapshot.text.replace(cid, new_cid);
    atomic_write(&dest, &text).map_err(|error| format!("副本正文写入失败：{error}"))?;
    match session_link::read_content_snapshot(&dest, new_cid) {
        ContentState::Ready(read_back)
            if read_back.normalized.total_digest == snapshot.normalized.total_digest =>
        {
            Ok(dest)
        }
        ContentState::Ready(_) => Err("副本正文写后校验不一致，未报告成功".to_string()),
        ContentState::Missing => Err("副本正文写入后不存在，未报告成功".to_string()),
        ContentState::Unavailable(reason) => Err(format!("副本正文写后无法验证：{reason}")),
    }
}

/// 首次使用时先落地空的关联存储。
///
/// 保证「操作日志出现」一定晚于「主文件存在」，否则刚预分配的操作日志会被
/// `load_store` 的残留痕迹规则误判成「主文件缺失但残留未完成操作」而自锁。
/// 存储损坏/未知版本/权限失败时同样在这里拒绝，绝不降级成空表。
fn ensure_link_store_ready(paths: &SessionPaths) -> Result<(), String> {
    match session_link::load_store(paths) {
        StoreState::Missing => {
            session_link::with_link_store_write(paths, |_| Ok(()))?;
            Ok(())
        }
        StoreState::Ready(_) => Ok(()),
        StoreState::Unavailable(reason) => Err(format!("{reason}；已阻止复制")),
    }
}

/// 复制单个会话：预分配 UUID → 持久化操作 → 正文 → 数据库 → 映射 → 关联/基线。
fn copy_one_session(context: &CopyContext, cid: &str) -> Result<CopyOutcome, String> {
    let paths = context.paths;
    // 上一次未完成的同一请求：只复用，不新建第二个副本。
    if let Some(operation) = session_link::find_pending_operation(
        context.pending,
        context.source_uid,
        cid,
        context.target_uid,
    ) {
        return Err(format!(
            "上一次复制尚未完成（操作 {}）：{}，本次未新建副本",
            operation.operation_id,
            operation
                .last_error
                .clone()
                .unwrap_or_else(|| "等待恢复".to_string())
        ));
    }

    let Some(source_path) = find_project_jsonl(paths, cid) else {
        return Err("会话正文不存在，未复制".to_string());
    };
    let snapshot = match session_link::read_content_snapshot(&source_path, cid) {
        ContentState::Ready(snapshot) => snapshot,
        ContentState::Missing => return Err("会话正文不存在，未复制".to_string()),
        ContentState::Unavailable(reason) => {
            return Err(format!("会话正文无法验证（{reason}），未复制"));
        }
    };
    let source_owner = session_row_owner(paths, cid);
    match source_owner.as_deref() {
        Some(owner) if owner == context.source_uid => {}
        Some(_) => return Err("源会话不属于当前账号，未复制".to_string()),
        None => return Err("数据库中找不到源会话记录，未复制".to_string()),
    }

    let resolution = resolve_target(context, cid)?;
    if let Some(session_id) = resolution.existing_link {
        return Ok(CopyOutcome::AlreadyLinked {
            session_id,
            group_id: resolution.group_id.unwrap_or_default(),
        });
    }

    // 预分配：任何副本写入之前先持久化目标 UUID，失败恢复复用同一个 UUID。
    ensure_link_store_ready(paths)?;
    let new_cid = uuid::Uuid::new_v4().to_string();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let group_id = resolution
        .group_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let backup_root = session_backup_root(paths, context.variant);
    let backup = backup_workbuddy_db(paths, &backup_root)?;

    let mut operation = Operation {
        version: OPERATION_VERSION,
        operation_id: operation_id.clone(),
        kind: "copy".to_string(),
        variant: context.variant,
        group_id: group_id.clone(),
        source: OperationMember {
            account_id: context.source_account_id.clone(),
            uid: context.source_uid.to_string(),
            session_id: cid.to_string(),
        },
        target: OperationMember {
            account_id: context.target_account_id.clone(),
            uid: context.target_uid.to_string(),
            session_id: new_cid.clone(),
        },
        expected_content_digest: snapshot.normalized.total_digest.clone(),
        expected_record_count: snapshot.normalized.record_count,
        phase: OpPhase::Prepared,
        backup: Some(backup.to_string_lossy().to_string()),
        last_error: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    session_link::save_operation(paths, &operation)?;

    if let Err(error) = finish_copy_from_body(
        paths,
        context.variant,
        &mut operation,
        &snapshot,
        Some(&source_path),
    ) {
        fail_operation(paths, &mut operation, &error);
        return Err(error);
    }
    let _ = session_link::prune_operations(
        paths,
        context.variant,
        session_link::KEEP_COMPLETED_OPERATIONS,
    );
    Ok(CopyOutcome::Copied {
        new_id: new_cid,
        group_id,
        backup: backup.to_string_lossy().to_string(),
    })
}

/// 从「源快照已确认」开始推进复制：写正文 → 写数据库行 → 登记映射 → 提交关联与基线。
///
/// `body_source` 为 `Some(source_path)` 时先写正文；为 `None` 表示正文此前已写成
/// （恢复场景），直接继续数据库行与关联。
fn finish_copy_from_body(
    paths: &SessionPaths,
    variant: WbVariant,
    operation: &mut Operation,
    snapshot: &ContentSnapshot,
    body_source: Option<&Path>,
) -> Result<(), String> {
    if let Some(source_path) = body_source {
        write_copy_body(
            snapshot,
            source_path,
            &operation.source.session_id,
            &operation.target.session_id,
        )?;
        advance_operation(paths, operation, OpPhase::BodyWritten)?;
    }

    match insert_session_copy(
        paths,
        &operation.target.session_id,
        &operation.source.session_id,
        &operation.source.uid,
        &operation.target.uid,
    )? {
        DbCopyOutcome::Inserted => {}
        DbCopyOutcome::SourceRowMissing => {
            return Err("数据库中找不到源会话记录，未复制".to_string())
        }
        DbCopyOutcome::NoSessionsTable => {
            return Err("会话数据库缺少 sessions 表，未复制".to_string())
        }
        DbCopyOutcome::NoDb => return Err("会话数据库不存在，未复制".to_string()),
    }
    verify_session_row(paths, &operation.target.session_id, &operation.target.uid)?;
    advance_operation(paths, operation, OpPhase::DbWritten)?;

    match register_edge_sync_mapping(
        paths,
        variant,
        &operation.target.session_id,
        &operation.target.uid,
    ) {
        MappingOutcome::Registered => {}
        MappingOutcome::Unavailable(reason) => return Err(reason),
    }
    advance_operation(paths, operation, OpPhase::MappingWritten)?;

    let group_id = commit_links(paths, variant, operation, &snapshot.normalized)?;
    operation.group_id = group_id;
    advance_operation(paths, operation, OpPhase::LinksCommitted)?;
    advance_operation(paths, operation, OpPhase::Completed)?;
    Ok(())
}

/// 推进操作阶段。阶段只能前进：已经走到更靠后的阶段时不回写（恢复路径不得把
/// `LinksCommitted`/`Completed` 写回 `DbWritten`）。
fn advance_operation(
    paths: &SessionPaths,
    operation: &mut Operation,
    phase: OpPhase,
) -> Result<(), String> {
    if operation.phase >= phase {
        return Ok(());
    }
    operation.phase = phase;
    operation.updated_at = now_ms();
    session_link::save_operation(paths, operation)
}

fn fail_operation(paths: &SessionPaths, operation: &mut Operation, error: &str) {
    operation.last_error = Some(error.to_string());
    operation.updated_at = now_ms();
    let _ = session_link::save_operation(paths, operation);
}

/// 原子提交关联与配对基线（含失效成员替换与基线继承）。
///
/// 整个读改写都在关联存储锁内完成；只有全部成功才推进 revision。
fn commit_links(
    paths: &SessionPaths,
    variant: WbVariant,
    operation: &Operation,
    normalized: &NormalizedContent,
) -> Result<String, String> {
    let source = &operation.source;
    let target = &operation.target;
    let group_id = operation.group_id.clone();
    let group_id_out = group_id.clone();
    session_link::with_link_store_write(paths, move |store| {
        let index = match store.groups.iter().position(|group| group.id == group_id) {
            Some(index) => index,
            None => {
                store.groups.push(LinkGroup {
                    id: group_id.clone(),
                    variant,
                    created_at: now_ms(),
                    members: Vec::new(),
                    pair_bases: Vec::new(),
                });
                store.groups.len() - 1
            }
        };
        let group = &mut store.groups[index];

        let source_member_id =
            match session_link::find_member(group, &source.uid, &source.session_id) {
                Some(member) => member.member_id.clone(),
                None => {
                    let member_id = uuid::Uuid::new_v4().to_string();
                    session_link::add_active_member(
                        group,
                        LinkMember {
                            member_id: member_id.clone(),
                            account_id: source.account_id.clone(),
                            uid: source.uid.clone(),
                            session_id: source.session_id.clone(),
                            state: MemberState::Active,
                            linked_at: now_ms(),
                            last_synced_at: None,
                        },
                    );
                    member_id
                }
            };

        let target_member_id =
            match session_link::find_member(group, &target.uid, &target.session_id) {
                Some(member) => {
                    let member_id = member.member_id.clone();
                    session_link::set_member_state(group, &member_id, MemberState::Active);
                    member_id
                }
                None => {
                    let member_id = uuid::Uuid::new_v4().to_string();
                    // 同账号的失效 active 成员在这里被显式 supersede，保留记录。
                    session_link::add_active_member(
                        group,
                        LinkMember {
                            member_id: member_id.clone(),
                            account_id: target.account_id.clone(),
                            uid: target.uid.clone(),
                            session_id: target.session_id.clone(),
                            state: MemberState::Active,
                            linked_at: now_ms(),
                            last_synced_at: None,
                        },
                    );
                    member_id
                }
            };

        // 本次复制的正文即来源与目标的共同基线（定向更新，不动其它配对）。
        let pair_baseline_ref = uuid::Uuid::new_v4().to_string();
        session_link::save_baseline(paths, &pair_baseline_ref, normalized)?;
        session_link::set_pair_base(
            group,
            &source_member_id,
            &target_member_id,
            &pair_baseline_ref,
            NORMALIZATION_VERSION,
        );

        // 继承：来源与组内其它成员已有的历史共同基线，只有在「来源内容有序前缀
        // 包含该基线」时才能建立到新成员的基线；已有配对基线不覆盖。
        let others: Vec<String> = group
            .members
            .iter()
            .filter(|member| {
                member.member_id != source_member_id && member.member_id != target_member_id
            })
            .map(|member| member.member_id.clone())
            .collect();
        for other_id in others {
            if session_link::find_pair_base(group, &other_id, &target_member_id).is_some() {
                continue;
            }
            let Some(pair) =
                session_link::find_pair_base(group, &source_member_id, &other_id).cloned()
            else {
                continue;
            };
            if let Some(record) =
                session_link::inheritable_baseline(paths, &pair, &normalized.line_digests)
            {
                session_link::set_pair_base(
                    group,
                    &other_id,
                    &target_member_id,
                    &record.baseline_ref,
                    pair.normalization_version,
                );
            }
        }
        Ok(group_id_out)
    })
}

/// 操作已标成 `LinksCommitted` 时，核验关联组与双方成员仍在（只读，不写存储）。
fn committed_links_present(paths: &SessionPaths, operation: &Operation) -> Result<(), String> {
    match session_link::load_store(paths) {
        StoreState::Ready(store) => {
            let Some(group) = store
                .groups
                .iter()
                .find(|group| group.id == operation.group_id)
            else {
                return Err("关联组缺失，已停止恢复".to_string());
            };
            let has_source = session_link::find_member(
                group,
                &operation.source.uid,
                &operation.source.session_id,
            )
            .is_some();
            let has_target = session_link::find_member(
                group,
                &operation.target.uid,
                &operation.target.session_id,
            )
            .is_some();
            if !has_source || !has_target {
                return Err("关联成员缺失，已停止恢复".to_string());
            }
            Ok(())
        }
        StoreState::Missing => Err("关联存储主文件缺失，已停止恢复".to_string()),
        StoreState::Unavailable(reason) => Err(reason),
    }
}

/// 当前账号库中按 uid 找账号 id（成员 accountId 仅作展示，身份判定仍以 uid 为准）。
fn account_id_for_uid(paths: &SessionPaths, uid: &str) -> Option<String> {
    let accounts = account::load_accounts_at(&account::accounts_file_in(&paths.store_root));
    accounts
        .iter()
        .find(|account| account.get("uid").and_then(Value::as_str) == Some(uid))
        .and_then(|account| account.get("id").and_then(Value::as_str))
        .map(String::from)
}

// ---------------------------------------------------------------------------
// 未完成操作恢复（design §4.2 / §4.5）
// ---------------------------------------------------------------------------

/// 恢复某档位全部未完成操作（需已持有档位操作锁）。
///
/// 恢复先检查实际状态再决定下一步，不盲目重放；中间产物被改动或丢失时只上报
/// needsRecovery，不覆盖未知内容。
pub fn recover_pending_session_operations_at(
    paths: &SessionPaths,
    variant: WbVariant,
) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let scan = session_link::scan_operations(paths);
    for problem in scan.problems {
        report.needs_recovery.push(RecoveryIssue {
            operation_id: problem.clone(),
            reason: format!(
                "{UNPARSEABLE_OPERATION_REASON}（{problem}），已停止恢复以免产生重复副本"
            ),
            retryable: false,
        });
    }
    for operation in scan
        .operations
        .into_iter()
        .filter(|operation| operation.variant == variant && operation.phase.is_unfinished())
    {
        match recover_operation(paths, variant, operation) {
            RecoverOutcome::Recovered(id) => report.recovered.push(id),
            RecoverOutcome::Abandoned(id) => report.abandoned.push(id),
            RecoverOutcome::NeedsRecovery {
                id,
                reason,
                retryable,
            } => {
                report.needs_recovery.push(RecoveryIssue {
                    operation_id: id,
                    reason,
                    retryable,
                });
            }
        }
    }
    report
}

/// 生产入口：自行获取档位操作锁后恢复（被占用时返回明确错误，不排队）。
pub fn recover_pending_session_operations(variant: WbVariant) -> Result<RecoveryReport, String> {
    let paths = SessionPaths::for_variant(variant);
    let _lock = session_link::try_acquire_variant_ops_lock(&paths, variant)?;
    Ok(recover_pending_session_operations_at(&paths, variant))
}

enum RecoverOutcome {
    Recovered(String),
    Abandoned(String),
    NeedsRecovery {
        id: String,
        reason: String,
        /// 重试可能成功（例如映射库暂不可用），不阻断账号切换。
        retryable: bool,
    },
}

/// 目标正文现状判定（恢复的第一步）。
enum BodyCheck {
    /// 正文与操作记录一致。
    Verified(NormalizedContent),
    /// 尚未写入（操作停在 Prepared 阶段）。
    Absent,
    /// 中间产物被改动/丢失：停止恢复。
    NeedsRecovery(String),
}

fn check_target_body(paths: &SessionPaths, operation: &Operation) -> BodyCheck {
    match find_project_jsonl(paths, &operation.target.session_id)
        .as_deref()
        .map(|path| session_link::read_content_snapshot(path, &operation.target.session_id))
    {
        Some(ContentState::Ready(content)) => {
            if content.normalized.total_digest == operation.expected_content_digest {
                BodyCheck::Verified(content.normalized)
            } else {
                BodyCheck::NeedsRecovery(
                    "目标正文与操作记录不一致（可能被其它程序改动），已停止恢复".to_string(),
                )
            }
        }
        Some(ContentState::Unavailable(reason)) => {
            BodyCheck::NeedsRecovery(format!("目标正文不可验证（{reason}），已停止恢复"))
        }
        Some(ContentState::Missing) | None => {
            if operation.phase >= OpPhase::BodyWritten {
                BodyCheck::NeedsRecovery("目标正文丢失，已停止恢复".to_string())
            } else {
                BodyCheck::Absent
            }
        }
    }
}

/// 恢复单个操作：按「持久化阶段 + 实际状态」逐阶段判断，已经越过的阶段不重放
/// （不重复登记映射、不重写关联存储与基线）；校验不因跳过重放而放松。
fn recover_operation(
    paths: &SessionPaths,
    variant: WbVariant,
    mut operation: Operation,
) -> RecoverOutcome {
    let operation_id = operation.operation_id.clone();
    let needs = |reason: String, retryable: bool| RecoverOutcome::NeedsRecovery {
        id: operation_id.clone(),
        reason,
        retryable,
    };

    // 1) 目标正文：已写成则直接复用；未写成则用当前源内容补写；被改动则停止。
    let normalized = match check_target_body(paths, &operation) {
        BodyCheck::Verified(normalized) => normalized,
        BodyCheck::NeedsRecovery(reason) => return needs(reason, false),
        BodyCheck::Absent => {
            let Some(source_path) = find_project_jsonl(paths, &operation.source.session_id) else {
                abandon_operation(paths, &mut operation);
                return RecoverOutcome::Abandoned(operation_id);
            };
            let source = match session_link::read_content_snapshot(
                &source_path,
                &operation.source.session_id,
            ) {
                ContentState::Ready(snapshot) => snapshot,
                _ => {
                    abandon_operation(paths, &mut operation);
                    return RecoverOutcome::Abandoned(operation_id);
                }
            };
            // 源内容在本机发生了变化：按当前内容继续（副本是快照复制，不是同步）。
            if source.normalized.total_digest != operation.expected_content_digest {
                operation.expected_content_digest = source.normalized.total_digest.clone();
                operation.expected_record_count = source.normalized.record_count;
            }
            if let Err(error) = write_copy_body(
                &source,
                &source_path,
                &operation.source.session_id,
                &operation.target.session_id,
            ) {
                fail_operation(paths, &mut operation, &error);
                return needs(error, true);
            }
            if let Err(error) = advance_operation(paths, &mut operation, OpPhase::BodyWritten) {
                return needs(error, true);
            }
            source.normalized
        }
    };

    // 2) 数据库行：缺失则补写，归属异常则停止。
    match session_row_owner(paths, &operation.target.session_id) {
        Some(owner) if owner == operation.target.uid => {}
        Some(_) => {
            return needs("目标会话行归属异常，已停止恢复".to_string(), false);
        }
        None => {
            if operation.phase >= OpPhase::DbWritten {
                return needs("目标会话行丢失，已停止恢复".to_string(), false);
            }
            match insert_session_copy(
                paths,
                &operation.target.session_id,
                &operation.source.session_id,
                &operation.source.uid,
                &operation.target.uid,
            ) {
                Ok(DbCopyOutcome::Inserted) => {}
                Ok(outcome) => {
                    let error = format!("会话行写入失败（{outcome:?}），保留操作待重试");
                    fail_operation(paths, &mut operation, &error);
                    return needs(error, false);
                }
                Err(error) => {
                    fail_operation(paths, &mut operation, &error);
                    return needs(error, true);
                }
            }
            if let Err(error) =
                verify_session_row(paths, &operation.target.session_id, &operation.target.uid)
            {
                return needs(error, false);
            }
        }
    }
    if let Err(error) = advance_operation(paths, &mut operation, OpPhase::DbWritten) {
        return needs(error, true);
    }

    // 3) 云端映射：已越过该阶段就不再重新登记（恢复不重放已完成的阶段）；
    // 但必须核验产物仍在，phase 写完、行却丢了时要报 needsRecovery，不能直接 Completed。
    if operation.phase < OpPhase::MappingWritten {
        match register_edge_sync_mapping(
            paths,
            variant,
            &operation.target.session_id,
            &operation.target.uid,
        ) {
            MappingOutcome::Registered => {}
            MappingOutcome::Unavailable(reason) => {
                fail_operation(paths, &mut operation, &reason);
                return needs(reason, true);
            }
        }
        if let Err(error) = advance_operation(paths, &mut operation, OpPhase::MappingWritten) {
            return needs(error, true);
        }
    } else if !mapping_row_matches(
        paths,
        variant,
        &operation.target.session_id,
        &operation.target.uid,
    ) {
        return needs("云端映射丢失或被改动，已停止恢复".to_string(), false);
    }

    // 4) 关联与基线：已提交过就不再 commit_links，避免重复写关联存储与基线；
    // 同样先核验组与成员仍在，不能把「跳过重放」当成「产物一定还在」。
    if operation.phase < OpPhase::LinksCommitted {
        match commit_links(paths, variant, &operation, &normalized) {
            Ok(group_id) => {
                operation.group_id = group_id;
            }
            Err(error) => {
                fail_operation(paths, &mut operation, &error);
                return needs(error, true);
            }
        }
        if let Err(error) = advance_operation(paths, &mut operation, OpPhase::LinksCommitted) {
            return needs(error, true);
        }
    } else if let Err(error) = committed_links_present(paths, &operation) {
        return needs(error, false);
    }
    if let Err(error) = advance_operation(paths, &mut operation, OpPhase::Completed) {
        return needs(error, true);
    }
    RecoverOutcome::Recovered(operation_id)
}

fn abandon_operation(paths: &SessionPaths, operation: &mut Operation) {
    operation.phase = OpPhase::Abandoned;
    operation.last_error = Some("源会话已不可用，且未写入任何副本，已放弃该操作".to_string());
    operation.updated_at = now_ms();
    let _ = session_link::save_operation(paths, operation);
}

#[cfg(test)]
mod tests {
    //! 会话复制/关联/恢复的端到端单测。
    //!
    //! 所有用例都在临时目录里构造数据根与存储根，绝不读写真实的 `~/.wb-switch`
    //! 或 WorkBuddy 数据目录。

    use super::*;
    use crate::modules::session_link::{LinkStore, MemberState, Operation, StoreState};
    use serde_json::json;

    struct Env {
        root: PathBuf,
        paths: SessionPaths,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "wb_switch_copy_test_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            let paths = SessionPaths {
                store_root: root.join("store"),
                data_root: root.join("data"),
                auth_file: root.join("auth.info"),
            };
            std::fs::create_dir_all(paths.projects_dir().join("ws-a")).unwrap();
            Env { root, paths }
        }

        fn paths(&self) -> SessionPaths {
            self.paths.clone()
        }

        fn set_login(&self, uid: &str) {
            std::fs::write(
                &self.paths.auth_file,
                json!({"account": {"uid": uid}}).to_string(),
            )
            .unwrap();
        }

        fn create_db(&self) {
            let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    title TEXT,
                    custom_title TEXT,
                    cwd TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    deleted_at INTEGER,
                    is_playground INTEGER
                );",
            )
            .unwrap();
        }

        fn create_edge_db(&self, variant: WbVariant) {
            let conn = Connection::open(self.paths.edge_sync_db(variant)).unwrap();
            conn.execute_batch(
                "CREATE TABLE edge_sync_mapping (
                    session_id TEXT,
                    conversation_id TEXT,
                    msg_channel TEXT,
                    created_at INTEGER
                );",
            )
            .unwrap();
        }

        fn add_session(&self, id: &str, uid: &str, title: &str) {
            let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, user_id, title, custom_title, cwd, created_at, updated_at, deleted_at, is_playground)
                 VALUES (?1, ?2, ?3, NULL, '/ws/a', 1000, 2000, NULL, 0)",
                rusqlite::params![id, uid, title],
            )
            .unwrap();
        }

        fn add_body(&self, cid: &str, text: &str) -> PathBuf {
            let path = self
                .paths
                .projects_dir()
                .join("ws-a")
                .join(format!("{cid}.jsonl"));
            std::fs::write(&path, text).unwrap();
            path
        }

        fn body_path(&self, cid: &str) -> PathBuf {
            self.paths
                .projects_dir()
                .join("ws-a")
                .join(format!("{cid}.jsonl"))
        }

        fn delete_row(&self, id: &str) {
            let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
            conn.execute("DELETE FROM sessions WHERE id = ?1", [id])
                .unwrap();
        }

        fn target(&self, uid: &str) -> Value {
            json!({"id": format!("acc-{uid}"), "uid": uid, "variant": "cn"})
        }

        fn store(&self) -> LinkStore {
            match session_link::load_store(&self.paths) {
                StoreState::Ready(store) => store,
                other => panic!("关联存储应为 Ready，实际 {other:?}"),
            }
        }

        fn body_files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.paths.projects_dir().join("ws-a"))
                .unwrap()
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    name.ends_with(".jsonl").then_some(name)
                })
                .collect();
            names.sort();
            names
        }

        /// 基线目录里的 `*.json` 数量（判断恢复是否新增基线）。
        fn baseline_files(&self) -> usize {
            std::fs::read_dir(self.paths.baselines_dir())
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                        .count()
                })
                .unwrap_or(0)
        }

        /// 云端映射表行数（判断恢复是否重复登记）。
        fn mapping_rows(&self) -> usize {
            let conn = Connection::open(self.paths.edge_sync_db(WbVariant::Cn)).unwrap();
            conn.query_row("SELECT COUNT(*) FROM edge_sync_mapping", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap() as usize
        }

        fn rows_for(&self, uid: &str) -> Vec<String> {
            let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
            let mut stmt = conn
                .prepare("SELECT id FROM sessions WHERE user_id = ?1 AND deleted_at IS NULL")
                .unwrap();
            let mut rows: Vec<String> = stmt
                .query_map([uid], |row| row.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect();
            rows.sort();
            rows
        }

        fn first_copy_id(&self, report: &Value) -> String {
            report["copied"][0]["newId"].as_str().unwrap().to_string()
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn body_text(cid: &str) -> String {
        format!(
            "{}\n{}\n",
            json!({"type": "user", "sessionId": cid, "text": "你好"}),
            json!({"type": "assistant", "sessionId": cid, "text": "hi"})
        )
    }

    /// 一个可用的国内版环境：源账号 uid-a 有一个带正文的会话 sess-1。
    fn ready_env(name: &str) -> Env {
        let env = Env::new(name);
        env.create_db();
        env.create_edge_db(WbVariant::Cn);
        env.set_login("uid-a");
        env.add_session("sess-1", "uid-a", "标题一");
        env.add_body("sess-1", &body_text("sess-1"));
        env
    }

    fn copy(env: &Env, target_uid: &str, ids: &[&str]) -> Value {
        let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        copy_sessions_for_switch_at(
            &env.paths(),
            WbVariant::Cn,
            &env.target(target_uid),
            &ids,
            |_| false,
        )
        .unwrap()
    }

    // ---------------------------------------------------------------------------
    // 基础路径与能力探测
    // ---------------------------------------------------------------------------

    #[test]
    fn db_paths_follow_variant_data_root() {
        let cn = SessionPaths::for_variant(WbVariant::Cn);
        assert!(cn
            .workbuddy_db()
            .to_string_lossy()
            .ends_with(".workbuddy/workbuddy.db"));
        assert!(cn
            .edge_sync_db(WbVariant::Cn)
            .to_string_lossy()
            .ends_with("edge-sync-mapping-v2.db"));

        let ai = SessionPaths::for_variant(WbVariant::Ai);
        assert_eq!(ai.workbuddy_db().parent(), Some(ai.data_root.as_path()));
        assert_ne!(cn.workbuddy_db(), ai.workbuddy_db());
        // 国际版实测为 v4 库，不能套用国内版 v2 文件名。
        assert!(ai
            .edge_sync_db(WbVariant::Ai)
            .to_string_lossy()
            .ends_with("edge-sync-mapping-v4.db"));
        assert_ne!(
            cn.edge_sync_db(WbVariant::Cn),
            ai.edge_sync_db(WbVariant::Ai)
        );
        // 锁与关联存储都挂在工具存储根下，顺序固定为「档位锁 → 存储锁」。
        assert!(cn
            .variant_ops_lock_file(WbVariant::Cn)
            .ends_with("locks/session-ops-cn.lock"));
        assert_ne!(
            cn.variant_ops_lock_file(WbVariant::Cn),
            cn.variant_ops_lock_file(WbVariant::Ai)
        );
        assert!(cn
            .link_store_lock_file()
            .ends_with("locks/session-links.lock"));
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wb_switch_session_root_{}_{name}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn create_sessions_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT, title TEXT);",
        )
        .unwrap();
    }

    /// 能力探测：`projects/` 目录 + `workbuddy.db` 的 `sessions` 表同时存在才可用。
    #[test]
    fn session_copy_capability_probe() {
        let bare = temp_root("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(!session_copy_supported_at(&bare));

        let only_projects = temp_root("only-projects");
        std::fs::create_dir_all(only_projects.join("projects")).unwrap();
        assert!(!session_copy_supported_at(&only_projects));

        let empty_db = temp_root("empty-db");
        std::fs::create_dir_all(&empty_db).unwrap();
        let conn = Connection::open(empty_db.join("workbuddy.db")).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();
        drop(conn);
        assert!(!session_copy_supported_at(&empty_db));

        let db_only = temp_root("db-only");
        std::fs::create_dir_all(&db_only).unwrap();
        create_sessions_db(&db_only.join("workbuddy.db"));
        assert!(!session_copy_supported_at(&db_only));

        let ready = temp_root("ready");
        std::fs::create_dir_all(ready.join("projects")).unwrap();
        create_sessions_db(&ready.join("workbuddy.db"));
        assert!(session_copy_supported_at(&ready));

        for dir in [bare, only_projects, empty_db, db_only, ready] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 能力不满足时返回明确错误，且不写任何文件。
    #[test]
    fn copy_sessions_for_switch_rejects_unsupported_root() {
        let env = Env::new("unsupported");
        let bare = Env {
            root: env.root.clone(),
            paths: SessionPaths {
                store_root: env.root.join("bare-store"),
                data_root: env.root.join("bare-data"),
                auth_file: env.root.join("auth.info"),
            },
        };
        std::fs::create_dir_all(bare.paths.data_root.clone()).unwrap();
        let err = copy_sessions_for_switch_at(
            &bare.paths(),
            WbVariant::Ai,
            &json!({"id": "ai-1", "uid": "u-ai", "variant": "ai"}),
            &["cid-1".to_string()],
            |_| false,
        )
        .expect_err("不支持的档位必须返回错误");
        assert!(err.contains(SESSION_COPY_UNSUPPORTED), "{err}");
        assert_eq!(std::fs::read_dir(&bare.paths.data_root).unwrap().count(), 0);
    }

    /// 能力探测只对国际版生效：国内版在探测不通过的数据根上仍走改造前的路径。
    #[test]
    fn session_copy_probe_only_gates_ai() {
        let env = Env::new("cn-no-probe");
        let bare = SessionPaths {
            store_root: env.root.join("bare-store"),
            data_root: env.root.join("bare-data"),
            auth_file: env.root.join("auth.info"),
        };
        std::fs::create_dir_all(&bare.data_root).unwrap();

        let cn_err = copy_sessions_for_switch_at(
            &bare,
            WbVariant::Cn,
            &json!({"id": "cn-1", "variant": "cn", "uid": "   "}),
            &["cid-1".to_string()],
            |_| false,
        )
        .expect_err("缺 uid 仍必须返回错误");
        assert_eq!(cn_err, "目标账号缺少 uid，无法复制会话");
        assert!(
            !cn_err.contains(SESSION_COPY_UNSUPPORTED),
            "国内版不得被能力探测拦截: {cn_err}"
        );

        let ai_err = copy_sessions_for_switch_at(
            &bare,
            WbVariant::Ai,
            &json!({"id": "ai-1", "uid": "u-ai", "variant": "ai"}),
            &["cid-1".to_string()],
            |_| false,
        )
        .expect_err("国际版不满足能力探测必须返回错误");
        assert!(ai_err.contains(SESSION_COPY_UNSUPPORTED), "{ai_err}");
    }

    /// 能力可用时继续走 uid 校验（证明探测不会误短路）。
    #[test]
    fn copy_sessions_for_switch_requires_target_uid() {
        let env = ready_env("requires-uid");
        let err = copy_sessions_for_switch_at(
            &env.paths(),
            WbVariant::Cn,
            &json!({"id": "a-1", "variant": "cn", "uid": "   "}),
            &["sess-1".to_string()],
            |_| false,
        )
        .expect_err("缺 uid 必须返回错误");
        assert_eq!(err, "目标账号缺少 uid，无法复制会话");
    }

    /// 未登录 / 目标即当前账号：拒绝且不写任何东西。
    #[test]
    fn copy_rejects_missing_login_and_same_account() {
        let env = ready_env("login-checks");
        std::fs::remove_file(&env.paths.auth_file).unwrap();
        let err = copy_sessions_for_switch_at(
            &env.paths(),
            WbVariant::Cn,
            &env.target("uid-b"),
            &["sess-1".to_string()],
            |_| false,
        )
        .expect_err("缺登录态必须报错");
        assert!(err.contains("未读取到本机登录态"), "{err}");

        env.set_login("uid-b");
        let err = copy_sessions_for_switch_at(
            &env.paths(),
            WbVariant::Cn,
            &env.target("uid-b"),
            &["sess-1".to_string()],
            |_| false,
        )
        .expect_err("同一账号必须报错");
        assert!(err.contains("当前账号与目标账号相同"), "{err}");
        assert_eq!(env.rows_for("uid-b").len(), 0);
    }

    // ---------------------------------------------------------------------------
    // 幂等复制与关联组（R1）
    // ---------------------------------------------------------------------------

    #[test]
    fn copy_writes_body_row_mapping_and_link_group() {
        let env = ready_env("happy");
        let report = copy(&env, "uid-b", &["sess-1"]);

        assert_eq!(report["sourceUid"], "uid-a");
        assert_eq!(report["targetUid"], "uid-b");
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
        assert_eq!(report["alreadyLinked"].as_array().unwrap().len(), 0);
        assert!(report.get("errors").is_none());
        assert!(report.get("needsRecovery").is_none());

        let new_id = env.first_copy_id(&report);
        assert_ne!(new_id, "sess-1");
        assert_eq!(report["copied"][0]["id"], "sess-1");

        // 正文：新 id 文件存在、旧 id 引用已替换，源文件不动。
        let copied_body = std::fs::read_to_string(env.body_path(&new_id)).unwrap();
        assert!(copied_body.contains(&new_id));
        assert!(!copied_body.contains("sess-1"));
        assert_eq!(
            std::fs::read_to_string(env.body_path("sess-1")).unwrap(),
            body_text("sess-1")
        );

        // 数据库行归属目标账号。
        assert_eq!(env.rows_for("uid-b"), vec![new_id.clone()]);
        assert_eq!(env.rows_for("uid-a"), vec!["sess-1".to_string()]);

        // 云端映射沿用既有登记：convmsg:{target_uid}。
        let conn = Connection::open(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();
        let channel: String = conn
            .query_row(
                "SELECT msg_channel FROM edge_sync_mapping WHERE session_id = ?1",
                [&new_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(channel, "convmsg:uid-b");

        // 关联组：同一逻辑会话、两个账号各一个 active 成员、一对基线。
        let store = env.store();
        assert!(
            store.revision >= 1,
            "首次落地空存储 + 关联提交都会推进 revision"
        );
        assert_eq!(store.groups.len(), 1);
        let group = &store.groups[0];
        assert_eq!(group.variant, WbVariant::Cn);
        assert_eq!(group.members.len(), 2);
        assert!(group
            .members
            .iter()
            .all(|member| member.state == MemberState::Active));
        assert_eq!(group.pair_bases.len(), 1);
        assert!(env
            .paths
            .baselines_dir()
            .join(format!("{}.json", group.pair_bases[0].baseline_ref))
            .exists());
        // 来源账号的成员带上了 accountId（账号库缺失时为 None，不影响身份判定）。
        assert!(group
            .members
            .iter()
            .any(|member| member.uid == "uid-a" && member.session_id == "sess-1"));
    }

    #[test]
    fn copy_retry_reuses_member_without_second_copy() {
        let env = ready_env("retry");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);
        let revision_before_retry = env.store().revision;

        let second = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(second["copied"].as_array().unwrap().len(), 0);
        assert_eq!(second["alreadyLinked"].as_array().unwrap().len(), 1);
        assert_eq!(second["alreadyLinked"][0]["sessionId"], new_id);
        assert!(second.get("errors").is_none());

        assert_eq!(env.body_files().len(), 2, "重试不得产生第二个副本");
        assert_eq!(env.rows_for("uid-b").len(), 1);
        assert_eq!(
            env.store().revision,
            revision_before_retry,
            "幂等复用不写关联存储"
        );
        assert_eq!(env.store().groups[0].members.len(), 2);
    }

    #[test]
    fn copy_back_reuses_original_session() {
        let env = ready_env("back");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);

        // 目标账号成为当前账号，把副本复制回原账号：必须复用原件。
        env.set_login("uid-b");
        let back = copy(&env, "uid-a", &[&new_id]);
        assert_eq!(back["copied"].as_array().unwrap().len(), 0);
        assert_eq!(back["alreadyLinked"].as_array().unwrap().len(), 1);
        assert_eq!(back["alreadyLinked"][0]["sessionId"], "sess-1");

        assert_eq!(env.body_files().len(), 2, "B→A 不得新建副本");
        assert_eq!(env.rows_for("uid-a"), vec!["sess-1".to_string()]);
        assert_eq!(env.store().groups[0].members.len(), 2);
    }

    #[test]
    fn chain_a_to_b_then_a_to_c_then_b_to_c_reuses_existing_copy() {
        let env = ready_env("chain");
        let to_b = copy(&env, "uid-b", &["sess-1"]);
        let b_id = env.first_copy_id(&to_b);

        // 同一来源再复制给 C：同组内新增一个成员，不是新组。
        let to_c = copy(&env, "uid-c", &["sess-1"]);
        let c_id = env.first_copy_id(&to_c);
        assert_eq!(env.store().groups.len(), 1);
        assert_eq!(env.store().groups[0].members.len(), 3);

        // B→C：组内已有 C 的有效副本，复用而不重复复制。
        env.set_login("uid-b");
        let b_to_c = copy(&env, "uid-c", &[&b_id]);
        assert_eq!(b_to_c["copied"].as_array().unwrap().len(), 0);
        assert_eq!(b_to_c["alreadyLinked"][0]["sessionId"], c_id);
        assert_eq!(env.body_files().len(), 3);

        // 配对基线按成员对保存：A/B、A/C 各有基线；C 加入时按 A/B 基线继承出 B/C。
        let group = &env.store().groups[0];
        assert_eq!(group.pair_bases.len(), 3);
        let member_id = |uid: &str| {
            group
                .members
                .iter()
                .find(|member| member.uid == uid)
                .unwrap()
                .member_id
                .clone()
        };
        let (a, b, c) = (member_id("uid-a"), member_id("uid-b"), member_id("uid-c"));
        let pair = |left: &str, right: &str| {
            session_link::find_pair_base(group, left, right)
                .unwrap_or_else(|| panic!("缺少成员对基线 {left}/{right}"))
                .baseline_ref
                .clone()
        };
        assert_eq!(pair(&a, &b), pair(&b, &c), "B/C 继承自 A/B 的共同基线");
        assert_ne!(pair(&a, &c), pair(&a, &b), "A/C 是各自新建的基线");
    }

    #[test]
    fn rename_does_not_break_link() {
        let env = ready_env("rename");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);

        // 用户在目标账号改名：关联不依赖标题。
        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute(
            "UPDATE sessions SET title = '改过的标题', custom_title = '自定义名' WHERE id = ?1",
            [&new_id],
        )
        .unwrap();
        drop(conn);

        let again = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(again["alreadyLinked"][0]["sessionId"], new_id);
        assert_eq!(env.body_files().len(), 2);
        assert_eq!(env.store().groups[0].members.len(), 2);
    }

    #[test]
    fn same_title_independent_sessions_stay_separate() {
        let env = ready_env("same-title");
        env.add_session("sess-2", "uid-a", "标题一");
        env.add_body("sess-2", &body_text("sess-2"));

        let report = copy(&env, "uid-b", &["sess-1", "sess-2"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 2);
        let ids: Vec<String> = report["copied"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["newId"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);

        // 同标题不建立关联：第二个会话各成一个组，且各自独立幂等。
        assert_eq!(env.store().groups.len(), 2);
        assert_eq!(env.rows_for("uid-b").len(), 2);
        assert_eq!(env.body_files().len(), 4);

        let again = copy(&env, "uid-b", &["sess-1", "sess-2"]);
        assert_eq!(again["alreadyLinked"].as_array().unwrap().len(), 2);
        assert_eq!(env.body_files().len(), 4);
    }

    #[test]
    fn invalid_target_member_is_superseded_and_rebuilt_without_resurrection() {
        let env = ready_env("rebuild");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let old_id = env.first_copy_id(&first);

        // 目标副本的正文丢失 → 旧成员失效，重建新成员。
        std::fs::remove_file(env.body_path(&old_id)).unwrap();
        let rebuilt = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(rebuilt["copied"].as_array().unwrap().len(), 1);
        let new_id = env.first_copy_id(&rebuilt);
        assert_ne!(new_id, old_id);
        assert_eq!(
            env.body_files().len(),
            2,
            "旧副本正文已删，只剩源与新建副本"
        );

        let group = &env.store().groups[0];
        assert_eq!(group.members.len(), 3);
        let actives: Vec<&str> = group
            .members
            .iter()
            .filter(|member| member.uid == "uid-b" && member.state == MemberState::Active)
            .map(|member| member.session_id.as_str())
            .collect();
        assert_eq!(actives, vec![new_id.as_str()], "每账号唯一 active");
        assert_eq!(
            group
                .members
                .iter()
                .find(|member| member.session_id == old_id)
                .unwrap()
                .state,
            MemberState::Superseded
        );

        // 旧正文恢复后不得自动争夺有效位置：仍是重建后的成员有效。
        env.add_body(&old_id, &body_text(&old_id));
        let after_restore = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(after_restore["alreadyLinked"][0]["sessionId"], new_id);
        assert_eq!(env.store().groups[0].members.len(), 3, "不新增成员");
    }

    #[test]
    fn identity_uses_uid_and_never_rebinds_by_account_id() {
        let env = ready_env("identity");

        // 手工写入一个组：目标成员 uid=uid-b，accountId 是旧账号 id（重新导入前的 id）。
        let paths = env.paths();
        session_link::with_link_store_write(&paths, |store| {
            store.groups.push(LinkGroup {
                id: "g-1".to_string(),
                variant: WbVariant::Cn,
                created_at: 1,
                members: vec![
                    LinkMember {
                        member_id: "m-a".to_string(),
                        account_id: Some("acc-uid-a".to_string()),
                        uid: "uid-a".to_string(),
                        session_id: "sess-1".to_string(),
                        state: MemberState::Active,
                        linked_at: 1,
                        last_synced_at: None,
                    },
                    LinkMember {
                        member_id: "m-b".to_string(),
                        account_id: Some("old-account-id".to_string()),
                        uid: "uid-b".to_string(),
                        session_id: "sess-b".to_string(),
                        state: MemberState::Active,
                        linked_at: 1,
                        last_synced_at: None,
                    },
                ],
                pair_bases: Vec::new(),
            });
            Ok(())
        })
        .unwrap();
        env.add_body("sess-b", &body_text("sess-b"));
        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, is_playground)
             VALUES ('sess-b', 'uid-b', '旧副本', '/ws/a', 1, 2, NULL, 0)",
            [],
        )
        .unwrap();
        drop(conn);

        // uid 相同、accountId 变了 → 仍复用（身份以 uid 为准）。
        let reused = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(reused["alreadyLinked"][0]["sessionId"], "sess-b");
        assert_eq!(env.body_files().len(), 2);

        // accountId 相同但 uid 不同 → 不得错误绑定，必须新建。
        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute(
            "UPDATE sessions SET user_id = 'uid-x' WHERE id = 'sess-b'",
            [],
        )
        .unwrap();
        drop(conn);
        let fresh = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(fresh["copied"].as_array().unwrap().len(), 1);
        assert_eq!(env.store().groups[0].members.len(), 3);
    }

    #[test]
    fn variant_isolation_keeps_groups_separate() {
        let env = ready_env("variants");
        let cn = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(cn["copied"].as_str(), None);
        assert_eq!(cn["copied"].as_array().unwrap().len(), 1);

        // 国际版数据根：单独一套数据（同 store 根），身份字符串相同但档位不同。
        let mut ai_paths = env.paths();
        ai_paths.data_root = env.root.join("data-ai");
        std::fs::create_dir_all(ai_paths.projects_dir().join("ws-a")).unwrap();
        let conn = Connection::open(ai_paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, title TEXT, custom_title TEXT, cwd TEXT, created_at INTEGER, updated_at INTEGER, deleted_at INTEGER, is_playground INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, is_playground)
             VALUES ('ai-sess-1', 'uid-a', 'AI 会话', '/ws/a', 1, 2, NULL, 0)",
            [],
        )
        .unwrap();
        drop(conn);
        std::fs::write(
            ai_paths.projects_dir().join("ws-a").join("ai-sess-1.jsonl"),
            body_text("ai-sess-1"),
        )
        .unwrap();
        let conn = Connection::open(ai_paths.edge_sync_db(WbVariant::Ai)).unwrap();
        conn.execute_batch(
            "CREATE TABLE edge_sync_mapping (session_id TEXT, conversation_id TEXT, msg_channel TEXT, created_at INTEGER);",
        )
        .unwrap();
        drop(conn);

        let ai_report = copy_sessions_for_switch_at(
            &ai_paths,
            WbVariant::Ai,
            &json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "ai"}),
            &["ai-sess-1".to_string()],
            |_| false,
        )
        .unwrap();
        assert_eq!(
            ai_report["copied"].as_array().unwrap().len(),
            1,
            "同档位身份不同，必须新复制"
        );

        let store = env.store();
        assert_eq!(store.groups.len(), 2);
        let variants: Vec<&str> = store
            .groups
            .iter()
            .map(|group| group.variant.as_str())
            .collect();
        assert!(
            variants.contains(&"cn") && variants.contains(&"ai"),
            "{variants:?}"
        );
        // 两档位不串数据：成员会话 id 不相交，国际版组里带着国际版来源会话。
        let mut seen = std::collections::HashSet::new();
        for group in &store.groups {
            for member in &group.members {
                assert!(
                    seen.insert(member.session_id.clone()),
                    "会话 {} 同时出现在两个档位的组里",
                    member.session_id
                );
            }
        }
        let ai_group = store
            .groups
            .iter()
            .find(|group| group.variant == WbVariant::Ai)
            .unwrap();
        assert!(ai_group
            .members
            .iter()
            .any(|member| member.session_id == "ai-sess-1"));
    }

    /// 报告契约：copied / alreadyLinked / errors 同时出现时字段完整（桌面与 webui 同形）。
    #[test]
    fn copy_report_carries_copied_already_linked_and_errors_together() {
        let env = ready_env("contract");
        env.add_session("sess-2", "uid-a", "标题二");
        env.add_body("sess-2", &body_text("sess-2"));

        let first = copy(&env, "uid-b", &["sess-1", "sess-2"]);
        assert_eq!(first["copied"].as_array().unwrap().len(), 2);

        // 第二个会话的正文丢失：本次一个复用、一个失败。
        std::fs::remove_file(env.body_path("sess-2")).unwrap();
        let mixed = copy(&env, "uid-b", &["sess-1", "sess-2"]);
        assert_eq!(mixed["sourceUid"], "uid-a");
        assert_eq!(mixed["targetUid"], "uid-b");
        assert_eq!(mixed["copied"].as_array().unwrap().len(), 0);
        assert_eq!(mixed["alreadyLinked"].as_array().unwrap().len(), 1);
        assert_eq!(mixed["alreadyLinked"][0]["id"], "sess-1");
        assert_eq!(mixed["errors"].as_array().unwrap().len(), 1);
        assert_eq!(mixed["errors"][0]["id"], "sess-2");
        assert_eq!(mixed["errors"][0]["error"], "会话正文不存在，未复制");
        // 复用/失败都不算未完成写入。
        assert!(mixed.get("needsRecovery").is_none());
    }

    // ---------------------------------------------------------------------------
    // 失败必须可见、可恢复（R1 / R5）
    // ---------------------------------------------------------------------------

    #[test]
    fn missing_body_is_not_reported_as_success() {
        let env = ready_env("no-body");
        std::fs::remove_file(env.body_path("sess-1")).unwrap();

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        assert_eq!(report["errors"][0]["error"], "会话正文不存在，未复制");
        assert_eq!(env.rows_for("uid-b").len(), 0, "不得写出半成品会话行");
        assert_eq!(env.body_files().len(), 0);
    }

    #[test]
    fn truncated_body_is_not_reported_as_success() {
        let env = ready_env("truncated");
        env.add_body("sess-1", "{\"sessionId\":\"sess-1\"\n");

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        let error = report["errors"][0]["error"].as_str().unwrap();
        assert!(error.contains("会话正文无法验证"), "{error}");
        assert_eq!(env.rows_for("uid-b").len(), 0);
    }

    #[test]
    fn missing_source_row_is_not_reported_as_success() {
        let env = ready_env("no-row");
        env.delete_row("sess-1");

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        assert_eq!(
            report["errors"][0]["error"],
            "数据库中找不到源会话记录，未复制"
        );
        assert_eq!(env.body_files().len(), 1, "不得写出无数据库行的正文");
    }

    #[test]
    fn source_row_of_another_account_is_rejected() {
        let env = ready_env("other-owner");
        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute(
            "UPDATE sessions SET user_id = 'uid-x' WHERE id = 'sess-1'",
            [],
        )
        .unwrap();
        drop(conn);

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        assert_eq!(report["errors"][0]["error"], "源会话不属于当前账号，未复制");
    }

    /// 映射登记失败 → 不报完整成功；修好后重试复用同一 UUID，不产生第二个副本。
    #[test]
    fn mapping_failure_keeps_pending_then_retry_reuses_same_uuid() {
        let env = ready_env("mapping");
        std::fs::remove_file(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();

        let failed = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(failed["copied"].as_array().unwrap().len(), 0);
        let error = failed["errors"][0]["error"].as_str().unwrap();
        assert!(error.contains("云端映射库"), "{error}");
        assert_eq!(failed["needsRecovery"], true);
        assert_eq!(
            env.body_files().len(),
            2,
            "正文与数据库行已写入，未报告成功"
        );

        let pending = session_link::pending_operations(&env.paths(), WbVariant::Cn);
        assert_eq!(pending.len(), 1);
        let new_id = pending[0].target.session_id.clone();
        assert!(pending[0].phase < crate::modules::session_link::OpPhase::Completed);

        // 映射库恢复后重试：恢复流程补齐，随后报告 alreadyLinked，UUID 不变。
        env.create_edge_db(WbVariant::Cn);
        let retry = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(retry["copied"].as_array().unwrap().len(), 0);
        assert_eq!(retry["alreadyLinked"][0]["sessionId"], new_id);
        assert!(retry.get("errors").is_none());
        assert!(session_link::pending_operations(&env.paths(), WbVariant::Cn).is_empty());
        assert_eq!(env.body_files().len(), 2, "恢复不得产生第二个副本");
        assert_eq!(env.rows_for("uid-b"), vec![new_id.clone()]);

        let conn = Connection::open(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();
        let channel: String = conn
            .query_row(
                "SELECT msg_channel FROM edge_sync_mapping WHERE session_id = ?1",
                [&new_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(channel, "convmsg:uid-b");
    }

    /// 关联提交失败（存储目录不可写）→ 不报成功；恢复后同一 UUID 完成。
    #[cfg(unix)]
    #[test]
    fn link_commit_failure_keeps_pending_then_recovers_with_same_uuid() {
        use std::os::unix::fs::PermissionsExt;

        let env = ready_env("link-commit");
        // 先跑一次成功，建立锁文件与存储文件，避免把「无法加锁」当成关联提交失败。
        copy(&env, "uid-c", &["sess-1"]);
        if std::fs::write(env.paths.store_root.join(".probe"), b"x").is_err() {
            return;
        }

        std::fs::set_permissions(
            &env.paths.store_root,
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        if std::fs::write(env.paths.store_root.join(".probe"), b"x").is_ok() {
            // root / 特殊 ACL 环境写保护无效：跳过（不误报）。
            std::fs::set_permissions(
                &env.paths.store_root,
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            return;
        }

        let failed = copy(&env, "uid-b", &["sess-1"]);
        std::fs::set_permissions(
            &env.paths.store_root,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(failed["copied"].as_array().unwrap().len(), 0);
        let error = failed["errors"][0]["error"].as_str().unwrap();
        assert!(
            error.contains("写入失败") || error.contains("关联存储"),
            "{error}"
        );
        assert_eq!(failed["needsRecovery"], true);

        let pending = session_link::pending_operations(&env.paths(), WbVariant::Cn);
        let target = pending
            .iter()
            .find(|operation| operation.target.uid == "uid-b")
            .expect("应保留未完成操作");
        let new_id = target.target.session_id.clone();

        let retry = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(retry["copied"].as_array().unwrap().len(), 0);
        assert_eq!(retry["alreadyLinked"][0]["sessionId"], new_id);
        assert_eq!(
            env.body_files().len(),
            4,
            "两个目标各一份副本，重试不得新增"
        );
        assert_eq!(env.rows_for("uid-b"), vec![new_id]);
    }

    /// 正文写入失败：不得写出数据库行，也不报成功。
    #[cfg(unix)]
    #[test]
    fn body_write_failure_reports_error_without_db_row() {
        use std::os::unix::fs::PermissionsExt;

        let env = ready_env("body-write");
        let ws = env.paths.projects_dir().join("ws-a");
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o555)).unwrap();
        let writable = std::fs::write(ws.join(".probe"), b"x").is_ok();
        let report = copy(&env, "uid-b", &["sess-1"]);
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            // root / 特殊 ACL 环境：写保护无效，跳过断言。
            return;
        }
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        let error = report["errors"][0]["error"].as_str().unwrap();
        assert!(error.contains("副本正文写入失败"), "{error}");
        assert_eq!(env.rows_for("uid-b").len(), 0);
        assert_eq!(env.body_files().len(), 1);
    }

    #[test]
    fn corrupt_store_blocks_copy_and_preserves_original() {
        let env = ready_env("corrupt-store");
        std::fs::create_dir_all(&env.paths.store_root).unwrap();
        std::fs::write(env.paths.session_links_file(), "not-json").unwrap();

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        let error = report["errors"][0]["error"].as_str().unwrap();
        assert!(
            error.contains("关联存储") && error.contains("已阻止复制"),
            "{error}"
        );
        assert_eq!(env.rows_for("uid-b").len(), 0);
        assert_eq!(env.body_files().len(), 1);
        assert_eq!(
            std::fs::read_to_string(env.paths.session_links_file()).unwrap(),
            "not-json",
            "必须保留现场，不得当空表覆盖"
        );
    }

    #[test]
    fn unknown_store_version_blocks_copy() {
        let env = ready_env("unknown-version");
        std::fs::create_dir_all(&env.paths.store_root).unwrap();
        std::fs::write(
            env.paths.session_links_file(),
            json!({"version": 99, "revision": 1, "groups": []}).to_string(),
        )
        .unwrap();

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        let error = report["errors"][0]["error"].as_str().unwrap();
        assert!(error.contains("版本"), "{error}");
        assert_eq!(env.rows_for("uid-b").len(), 0);
    }

    /// 主文件被删但基线文件仍在 → 检测到痕迹，不得当首次使用重建空表，复制被阻止。
    #[test]
    fn copy_blocked_when_store_file_missing_but_baselines_remain() {
        let env = ready_env("baseline-trace");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);
        assert!(env.baseline_files() > 0, "首次复制应留下基线文件");

        std::fs::remove_file(env.paths.session_links_file()).unwrap();
        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        assert_eq!(report["alreadyLinked"].as_array().unwrap().len(), 0);
        let error = report["errors"][0]["error"].as_str().unwrap();
        assert!(error.contains("保留现场"), "{error}");
        assert!(
            !env.paths.session_links_file().exists(),
            "不得重建空表覆盖现场"
        );
        assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
        assert_eq!(env.rows_for("uid-b"), vec![new_id]);
    }

    /// 并发请求：档位操作锁被占用时直接拒绝，不产生第二个副本。
    #[test]
    fn concurrent_request_is_rejected_without_second_copy() {
        let env = ready_env("concurrent");
        let held = session_link::try_acquire_variant_ops_lock(&env.paths(), WbVariant::Cn).unwrap();

        let err = copy_sessions_for_switch_at(
            &env.paths(),
            WbVariant::Cn,
            &env.target("uid-b"),
            &["sess-1".to_string()],
            |_| false,
        )
        .expect_err("持锁期间必须拒绝");
        assert!(err.contains("会话操作"), "{err}");
        assert_eq!(env.body_files().len(), 1);

        drop(held);
        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
    }

    /// 拿档位锁后复查 App 是否运行：锁前未运行、拿锁后已被启动 → 拒绝且不写任何产物。
    #[test]
    fn copy_rechecks_app_running_after_acquiring_lock() {
        let env = ready_env("app-raced");
        let probes = std::cell::Cell::new(0usize);
        let lock_held_on_recheck = std::cell::Cell::new(false);
        let lock_path = env.paths.variant_ops_lock_file(WbVariant::Cn);
        let probe = |_: WbVariant| {
            let n = probes.get() + 1;
            probes.set(n);
            if n == 1 {
                false
            } else {
                // 第二次必须发生在持锁之后：此时再抢同一把锁应为 Busy。
                match session_link::try_lock_file(&lock_path) {
                    Err(session_link::LockError::Busy) => lock_held_on_recheck.set(true),
                    Err(session_link::LockError::Unavailable(reason)) => {
                        panic!("第二次探针时期望档位锁已被持有，实际 Unavailable: {reason}")
                    }
                    Ok(_) => panic!("第二次探针时期望档位锁已被持有，实际拿到了锁"),
                }
                true
            }
        };
        let err = copy_sessions_for_switch_at(
            &env.paths(),
            WbVariant::Cn,
            &env.target("uid-b"),
            &["sess-1".to_string()],
            probe,
        )
        .expect_err("拿锁后复查为运行中必须拒绝");
        assert_eq!(err, SESSION_COPY_APP_RUNNING);
        assert_eq!(probes.get(), 2, "锁前与锁后各检查一次");
        assert!(
            lock_held_on_recheck.get(),
            "复查必须发生在已经拿到档位锁之后、任何写入之前"
        );
        assert_eq!(env.body_files().len(), 1, "不得写入副本正文");
        assert_eq!(env.rows_for("uid-b").len(), 0, "不得写入数据库行");
        assert!(
            !env.paths.session_links_file().exists(),
            "不得初始化关联存储"
        );
        assert!(session_link::pending_operations(&env.paths(), WbVariant::Cn).is_empty());

        // 锁已释放：探针改回「未运行」后复制照常成功。
        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
    }

    // ---------------------------------------------------------------------------
    // 恢复
    // ---------------------------------------------------------------------------

    #[test]
    fn recovery_abandons_prepared_operation_when_source_is_gone() {
        let env = ready_env("abandon");
        let paths = env.paths();
        session_link::save_operation(
            &paths,
            &Operation {
                version: crate::modules::session_link::OPERATION_VERSION,
                operation_id: "op-gone".to_string(),
                kind: "copy".to_string(),
                variant: WbVariant::Cn,
                group_id: "g-gone".to_string(),
                source: OperationMember {
                    account_id: None,
                    uid: "uid-a".to_string(),
                    session_id: "sess-gone".to_string(),
                },
                target: OperationMember {
                    account_id: None,
                    uid: "uid-b".to_string(),
                    session_id: "new-gone".to_string(),
                },
                expected_content_digest: "d".to_string(),
                expected_record_count: 1,
                phase: crate::modules::session_link::OpPhase::Prepared,
                backup: None,
                last_error: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();

        let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
        assert_eq!(report.abandoned, vec!["op-gone".to_string()]);
        assert!(report.is_clean());
        assert!(session_link::pending_operations(&paths, WbVariant::Cn).is_empty());
        assert_eq!(env.body_files().len(), 1);
    }

    #[test]
    fn recovery_stops_when_intermediate_body_was_modified() {
        let env = ready_env("recovery-stop");
        std::fs::remove_file(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();
        let failed = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(failed["needsRecovery"], true);
        let new_id = session_link::pending_operations(&env.paths(), WbVariant::Cn)[0]
            .target
            .session_id
            .clone();

        // 中间产物被其它程序改动 → 停止恢复，不覆盖。
        let tampered = format!(
            "{}\n",
            json!({"type": "user", "sessionId": new_id, "text": "别人改的"})
        );
        std::fs::write(env.body_path(&new_id), &tampered).unwrap();

        let report = recover_pending_session_operations_at(&env.paths(), WbVariant::Cn);
        assert!(!report.is_clean());
        let issue = &report.needs_recovery[0];
        assert!(!issue.retryable);
        assert!(
            issue.reason.contains("目标正文与操作记录不一致"),
            "{}",
            issue.reason
        );
        assert_eq!(
            std::fs::read_to_string(env.body_path(&new_id)).unwrap(),
            tampered
        );

        // 该会话再次请求时不新建副本，而是报告未完成。
        env.create_edge_db(WbVariant::Cn);
        let again = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(again["copied"].as_array().unwrap().len(), 0);
        assert_eq!(again["errors"].as_array().unwrap().len(), 1);
        assert!(again["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("上一次复制尚未完成"));
        assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
    }

    #[test]
    fn retry_after_partial_copy_does_not_duplicate() {
        let env = ready_env("partial-resume");
        std::fs::remove_file(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();
        copy(&env, "uid-b", &["sess-1"]);
        let body_count_after_failure = env.body_files().len();

        // 未修复映射库就再次请求：不得新建副本。
        let again = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(again["copied"].as_array().unwrap().len(), 0);
        assert_eq!(env.body_files().len(), body_count_after_failure);

        // 修好后恢复完成。
        env.create_edge_db(WbVariant::Cn);
        let retry = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(retry["alreadyLinked"].as_array().unwrap().len(), 1);
        assert_eq!(env.body_files().len(), body_count_after_failure);
        assert_eq!(env.rows_for("uid-b").len(), 1);
    }

    /// 恢复不重放已完成的阶段：产物全部就位、只剩 Completed 未写时，只补写阶段标记，
    /// 不重写关联存储与基线、不重复登记映射（design §5）。
    #[test]
    fn recovery_completes_without_replaying_finished_stages() {
        let env = ready_env("recover-no-replay");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);
        let paths = env.paths();

        // 模拟「关联已提交、Completed 写入失败」：把已完成的操作日志回退到 LinksCommitted。
        let mut operation = session_link::scan_operations(&paths)
            .operations
            .into_iter()
            .find(|operation| operation.target.session_id == new_id)
            .expect("应能找到该副本的操作记录");
        operation.phase = OpPhase::LinksCommitted;
        session_link::save_operation(&paths, &operation).unwrap();
        let operation_id = operation.operation_id.clone();

        let before = env.store();
        let revision_before = before.revision;
        let pair_bases_before = before.groups[0].pair_bases.clone();
        let baselines_before = env.baseline_files();
        let members_before = before.groups[0].members.len();

        let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
        assert_eq!(report.recovered, vec![operation_id]);
        assert!(report.is_clean(), "{:?}", report.needs_recovery);

        let after = env.store();
        assert_eq!(after.revision, revision_before, "恢复不得重写关联存储");
        assert_eq!(after.groups[0].pair_bases.len(), pair_bases_before.len());
        assert_eq!(
            after.groups[0].pair_bases[0].baseline_ref, pair_bases_before[0].baseline_ref,
            "不得重写配对基线"
        );
        assert_eq!(after.groups[0].members.len(), members_before);
        assert_eq!(env.baseline_files(), baselines_before, "不得新增基线文件");
        assert_eq!(env.mapping_rows(), 1, "不得重复登记云端映射");
        assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
        assert_eq!(env.rows_for("uid-b"), vec![new_id]);
        assert!(session_link::pending_operations(&paths, WbVariant::Cn).is_empty());
    }

    /// phase 已是 LinksCommitted，但关联主文件被删：不得跳过核验后标 Completed。
    #[test]
    fn recovery_stops_when_committed_links_are_missing() {
        let env = ready_env("recover-links-gone");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);
        let paths = env.paths();

        let mut operation = session_link::scan_operations(&paths)
            .operations
            .into_iter()
            .find(|operation| operation.target.session_id == new_id)
            .expect("应能找到该副本的操作记录");
        operation.phase = OpPhase::LinksCommitted;
        session_link::save_operation(&paths, &operation).unwrap();
        std::fs::remove_file(paths.session_links_file()).unwrap();

        let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
        assert!(report.recovered.is_empty(), "{:?}", report.recovered);
        assert_eq!(report.needs_recovery.len(), 1);
        assert!(!report.needs_recovery[0].retryable);
        assert!(
            report.needs_recovery[0].reason.contains("关联"),
            "{}",
            report.needs_recovery[0].reason
        );
        assert!(
            !paths.session_links_file().exists(),
            "不得把缺失的主文件当成空表重建"
        );
        assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
        assert_eq!(
            session_link::pending_operations(&paths, WbVariant::Cn).len(),
            1,
            "必须保留未完成操作，不能标 Completed"
        );
    }

    /// phase 已越过 MappingWritten，但映射行被删：不得跳过核验后标 Completed。
    #[test]
    fn recovery_stops_when_mapping_row_is_missing() {
        let env = ready_env("recover-mapping-gone");
        let first = copy(&env, "uid-b", &["sess-1"]);
        let new_id = env.first_copy_id(&first);
        let paths = env.paths();

        let mut operation = session_link::scan_operations(&paths)
            .operations
            .into_iter()
            .find(|operation| operation.target.session_id == new_id)
            .expect("应能找到该副本的操作记录");
        operation.phase = OpPhase::LinksCommitted;
        session_link::save_operation(&paths, &operation).unwrap();

        let conn = Connection::open(paths.edge_sync_db(WbVariant::Cn)).unwrap();
        conn.execute(
            "DELETE FROM edge_sync_mapping WHERE session_id = ?1",
            [&new_id],
        )
        .unwrap();
        drop(conn);

        let revision_before = env.store().revision;
        let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
        assert!(report.recovered.is_empty(), "{:?}", report.recovered);
        assert_eq!(report.needs_recovery.len(), 1);
        assert!(!report.needs_recovery[0].retryable);
        assert!(
            report.needs_recovery[0].reason.contains("云端映射"),
            "{}",
            report.needs_recovery[0].reason
        );
        assert_eq!(env.store().revision, revision_before, "不得重写关联存储");
        assert_eq!(env.mapping_rows(), 0, "不得悄悄补登记映射");
        assert_eq!(
            session_link::pending_operations(&paths, WbVariant::Cn).len(),
            1
        );
    }

    // ---------------------------------------------------------------------------
    // 账户/环境辅助
    // ---------------------------------------------------------------------------

    #[test]
    fn unparseable_operation_log_blocks_copy_without_second_replica() {
        let env = ready_env("bad-op-json");
        std::fs::create_dir_all(env.paths.operations_dir()).unwrap();
        std::fs::write(env.paths.operations_dir().join("broken.json"), "not-json").unwrap();

        let recovery = recover_pending_session_operations_at(&env.paths(), WbVariant::Cn);
        assert!(!recovery.is_clean());
        assert_eq!(recovery.needs_recovery.len(), 1);
        assert!(!recovery.needs_recovery[0].retryable);
        assert!(
            recovery.needs_recovery[0]
                .reason
                .contains(UNPARSEABLE_OPERATION_REASON),
            "{}",
            recovery.needs_recovery[0].reason
        );

        let report = copy(&env, "uid-b", &["sess-1"]);
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        assert_eq!(report["alreadyLinked"].as_array().unwrap().len(), 0);
        assert_eq!(report["needsRecovery"], true);
        let error = report["errors"][0]["error"].as_str().unwrap();
        assert!(error.contains(UNPARSEABLE_OPERATION_REASON), "{error}");
        assert_eq!(
            env.body_files().len(),
            1,
            "不得绕过损坏的操作记录写出第二个副本"
        );
        assert_eq!(env.rows_for("uid-b").len(), 0);
    }

    #[test]
    fn recovery_reports_store_unavailable_instead_of_writing() {
        let env = ready_env("store-broken-recovery");
        std::fs::create_dir_all(&env.paths.store_root).unwrap();
        std::fs::write(env.paths.session_links_file(), "not-json").unwrap();

        let report = recover_pending_session_operations_at(&env.paths(), WbVariant::Cn);
        assert!(report.is_clean(), "没有未完成操作时不做任何事");
        assert_eq!(
            std::fs::read_to_string(env.paths.session_links_file()).unwrap(),
            "not-json"
        );
    }

    #[test]
    fn session_display_title_prefers_custom_title() {
        assert_eq!(
            session_display_title(Some("自动标题".into()), Some("美团每日自动领券".into())),
            "美团每日自动领券"
        );
        assert_eq!(
            session_display_title(None, Some("美团每日自动领券".into())),
            "美团每日自动领券"
        );
        assert_eq!(
            session_display_title(Some("汉字详情页".into()), None),
            "汉字详情页"
        );
        assert_eq!(session_display_title(None, None), "(无标题)");
        assert_eq!(
            session_display_title(Some("  ".into()), Some("".into())),
            "(无标题)"
        );
    }

    #[test]
    fn claw_workspace_detected_by_folder_name() {
        assert!(is_claw_workspace("/Users/apple/WorkBuddy/Claw"));
        assert!(is_claw_workspace("/Users/apple/WorkBuddy/claw/"));
        assert!(is_claw_workspace(r"C:\Users\me\WorkBuddy\Claw"));
        assert!(!is_claw_workspace("/Users/apple/WorkBuddy/ClawBot"));
        assert!(!is_claw_workspace(
            "/Users/apple/Documents/AI-PROJECT/LetterTotTown"
        ));
    }

    #[test]
    fn list_sessions_marks_has_history() {
        let env = ready_env("list-sessions");
        let sessions = list_sessions_for_user_at(&env.paths(), "uid-a");
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        assert_eq!(sessions[0]["id"], "sess-1");
        assert_eq!(sessions[0]["hasHistory"], true);

        std::fs::remove_file(env.body_path("sess-1")).unwrap();
        let sessions = list_sessions_for_user_at(&env.paths(), "uid-a");
        assert_eq!(sessions[0]["hasHistory"], false);
    }

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wb_switch_test_{}_{name}.db",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn insert_session_copy_duplicates_row_with_target_uid() {
        let env = ready_env("insert");
        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, is_playground)
             VALUES ('src-1', 'uid-a', '旧标题', '/ws', 1000, 2000, NULL, 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let outcome =
            insert_session_copy(&env.paths(), "new-uuid-1", "src-1", "uid-a", "uid-b").unwrap();
        assert_eq!(outcome, DbCopyOutcome::Inserted);

        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        let (id, user_id, title, deleted_at, is_playground): (String, String, String, Option<i64>, i64) =
            conn.query_row(
                "SELECT id, user_id, title, deleted_at, is_playground FROM sessions WHERE id = 'new-uuid-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(id, "new-uuid-1");
        assert_eq!(user_id, "uid-b");
        assert_eq!(title, "旧标题"); // 普通列原样保留
        assert_eq!(deleted_at, None);
        assert_eq!(is_playground, 0);
    }

    #[test]
    fn insert_session_copy_reports_missing_source_and_db() {
        let env = ready_env("insert-missing");
        assert_eq!(
            insert_session_copy(&env.paths(), "new-1", "missing", "uid-a", "uid-b").unwrap(),
            DbCopyOutcome::SourceRowMissing,
            "源行缺失必须显式上报，不能当成功（旧实现的假成功）"
        );

        std::fs::remove_file(env.paths.workbuddy_db()).unwrap();
        assert_eq!(
            insert_session_copy(&env.paths(), "new-1", "sess-1", "uid-a", "uid-b").unwrap(),
            DbCopyOutcome::NoDb
        );
    }

    #[test]
    fn register_edge_sync_mapping_reports_unavailable_reasons() {
        let env = ready_env("edge-outcome");
        // 缺表。
        let db = temp_db("edge-no-table");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();
        drop(conn);
        let paths = SessionPaths {
            store_root: env.root.join("s2"),
            data_root: temp_db("edge-root"),
            auth_file: env.root.join("auth2.info"),
        };
        std::fs::create_dir_all(&paths.data_root).unwrap();
        std::fs::copy(&db, paths.edge_sync_db(WbVariant::Cn)).unwrap();
        assert!(matches!(
            register_edge_sync_mapping(&paths, WbVariant::Cn, "new-1", "uid-b"),
            MappingOutcome::Unavailable(_)
        ));
        let _ = std::fs::remove_file(&db);

        // 正常登记。
        assert!(matches!(
            register_edge_sync_mapping(&env.paths(), WbVariant::Cn, "new-1", "uid-b"),
            MappingOutcome::Registered
        ));
        let conn = Connection::open(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();
        let (sid, cid, channel): (String, String, String) = conn
            .query_row(
                "SELECT session_id, conversation_id, msg_channel FROM edge_sync_mapping",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(sid, "new-1");
        assert_eq!(cid, "new-1");
        assert_eq!(channel, "convmsg:uid-b");
    }

    #[test]
    fn backup_failure_is_propagated_instead_of_claimed_success() {
        let env = ready_env("backup-fail");
        std::fs::remove_file(env.paths.workbuddy_db()).unwrap();
        let err = backup_workbuddy_db(&env.paths(), &env.paths.backup_root()).unwrap_err();
        assert!(err.contains("会话数据库不存在"), "{err}");

        // 正常备份返回主库路径且大小一致。
        env.create_db();
        env.add_session("sess-1", "uid-a", "标题一");
        let backup = backup_workbuddy_db(&env.paths(), &env.paths.backup_root()).unwrap();
        assert!(backup.ends_with("workbuddy.db"));
        assert_eq!(
            std::fs::metadata(&backup).unwrap().len(),
            std::fs::metadata(env.paths.workbuddy_db()).unwrap().len()
        );
    }

    #[test]
    fn session_row_owner_reads_target_uid() {
        let env = ready_env("row-owner");
        assert_eq!(
            session_row_owner(&env.paths(), "sess-1").as_deref(),
            Some("uid-a")
        );
        assert_eq!(session_row_owner(&env.paths(), "missing"), None);
        env.delete_row("sess-1");
        assert_eq!(session_row_owner(&env.paths(), "sess-1"), None);
    }
}
