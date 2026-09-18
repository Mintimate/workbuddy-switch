//! 会话复制关联组、配对基线与操作日志（design §2 / §3.1 / §4 的存储与内容层）。
//!
//! 落点（全部位于工具存储根 `~/.wb-switch` 下，路径由 [`SessionPaths`] 注入）：
//!   - `session_links.json`                  关联组主表（version + revision + groups）
//!   - `session-links/baselines/{ref}.json`  配对基线（有序行摘要 + 总摘要 + 记录数）
//!   - `session-links/operations/{id}.json`  复制操作日志（阶段 + 预分配目标 UUID）
//!   - `locks/session-ops-{variant}.lock`    档位操作锁（跨进程，覆盖整个会话操作）
//!   - `locks/session-links.lock`            关联存储短时全局锁（读改写在锁内进行）
//!
//! 锁顺序固定为「档位操作锁 → 关联存储锁」，任何调用方不得反向获取，避免死锁。
//! `atomic_write` 只防半写，不能代替互斥；关联的读改写必须在存储锁内完成。
//!
//! 读取语义区分 Missing / Ready / Unavailable：只有「首次使用且没有任何未完成痕迹」
//! 才按 Missing 初始化；损坏、权限失败、未知版本、主文件缺失但残留未完成操作或基线
//! 一律 Unavailable——保留现场并禁止写入，不得降级成空表后保存。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::modules::config::{atomic_write, now_ms};
use crate::modules::session::SessionPaths;
use crate::modules::variant::WbVariant;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 关联组主表格式版本；读到其它版本一律禁写（保留现场）。
pub const LINK_STORE_VERSION: u32 = 1;
/// 正文归一化规则版本；规则变化必须递增（design §3.1）。
pub const NORMALIZATION_VERSION: u32 = 1;
/// 配对基线记录格式版本。
pub const BASELINE_VERSION: u32 = 1;
/// 操作日志格式版本。
pub const OPERATION_VERSION: u32 = 1;
/// 归一化时替代「本副本 sessionId」的固定标记。其余 sessionId 一律保留原文。
pub const SESSION_ID_MARKER: &str = "__wb_switch_session_id__";
/// 每档位保留的已完成操作日志条数（未完成的一律保留）。
pub const KEEP_COMPLETED_OPERATIONS: usize = 20;
/// 存储锁的最长等待（存储锁只用于短时读改写）。
const STORE_LOCK_RETRY: usize = 25;
const STORE_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// 内容版本（design §3.1）
// ---------------------------------------------------------------------------

/// 归一化后的有序内容身份：逐行摘要 + 总摘要 + 记录数。
///
/// 行摘要对「长度编码 + 归一化行字节」取 SHA-256，记录分隔无歧义；行顺序、重复次数、
/// 键序都参与摘要，因此重排、去重、重写都会产生不同的内容身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedContent {
    pub record_count: usize,
    pub line_digests: Vec<String>,
    pub total_digest: String,
}

/// 一次成功读取的正文快照。
///
/// `text` 与摘要来自同一次读取：复制时直接写这一份，避免二次读取产生的 TOCTOU；
/// `full_digest` 覆盖原始字节（含空白），供之后的预览版本校验使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentSnapshot {
    pub text: String,
    pub full_digest: String,
    pub normalized: NormalizedContent,
}

/// 正文读取结果。`Unavailable` 表示内容不可验证（空/非法/读取期间变化），
/// 调用方不得据此快进或覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentState {
    Missing,
    Ready(ContentSnapshot),
    Unavailable(String),
}

/// 原始字节摘要（含空白与换行）。
pub fn full_digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// 由有序行摘要推导总摘要（前缀可自洽重算，便于校验基线未被篡改）。
pub fn total_digest_of(line_digests: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"wb-switch-lines-v1\0");
    hasher.update((line_digests.len() as u64).to_be_bytes());
    for digest in line_digests {
        hasher.update(digest.as_bytes());
        hasher.update([0u8]);
    }
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 单行摘要在长度编码后计算：记录分隔无歧义（design §3.1）。
fn line_digest_of(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut hasher = Sha256::new();
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// 逐行校验 JSONL 完整性并按本副本 sessionId 归一。
///
/// - 空正文、空行、非法 JSON 一律返回 Err（design：未知一律 Unknown，不快进）；
/// - 保留记录顺序与重复次数，不按记录 id 去重，不忽略未知字段；
/// - 仅把本副本自己的 sessionId 文本替换为固定标记，其它 sessionId 原样保留
///   （沿用既有复制的引用替换语义）；键序与格式差异因此会成为保守差异。
pub fn normalize_jsonl(text: &str, own_session_id: &str) -> Result<NormalizedContent, String> {
    let own_session_id = own_session_id.trim();
    if own_session_id.is_empty() {
        return Err("缺少会话 id，无法归一化正文".to_string());
    }
    if text.trim().is_empty() {
        return Err("正文为空".to_string());
    }

    let mut lines: Vec<&str> = text.split('\n').collect();
    // 末尾换行（含多个）不算记录；中间空行仍视为异常。
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    let mut line_digests = Vec::with_capacity(lines.len());
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            return Err(format!("第 {} 行为空，正文可能被截断", index + 1));
        }
        serde_json::from_str::<serde_json::Value>(line)
            .map_err(|_| format!("第 {} 行不是合法 JSON，正文可能被截断", index + 1))?;
        line_digests.push(line_digest_of(
            &line.replace(own_session_id, SESSION_ID_MARKER),
        ));
    }
    let total_digest = total_digest_of(&line_digests);
    Ok(NormalizedContent {
        record_count: line_digests.len(),
        line_digests,
        total_digest,
    })
}

/// 读取正文快照并做完整性校验；读取期间文件被改动时判定为不可验证。
pub fn read_content_snapshot(path: &Path, own_session_id: &str) -> ContentState {
    let before = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return ContentState::Missing,
        Err(error) => return ContentState::Unavailable(format!("正文无法读取：{error}")),
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return ContentState::Missing,
        Err(error) => return ContentState::Unavailable(format!("正文读取失败：{error}")),
    };
    let after = std::fs::metadata(path);
    let changed = match after {
        Ok(meta) => meta.len() != before.len() || meta.modified().ok() != before.modified().ok(),
        Err(_) => true,
    };
    if changed {
        return ContentState::Unavailable("正文在读取期间发生变化，未确认完整".to_string());
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return ContentState::Unavailable("正文不是合法 UTF-8".to_string()),
    };
    let full_digest = full_digest_of(text.as_bytes());
    match normalize_jsonl(&text, own_session_id) {
        Ok(normalized) => ContentState::Ready(ContentSnapshot {
            text,
            full_digest,
            normalized,
        }),
        Err(reason) => ContentState::Unavailable(reason),
    }
}

/// `prefix` 是否为 `full` 的有序前缀（用于基线继承校验）。
pub fn is_ordered_prefix(prefix: &[String], full: &[String]) -> bool {
    prefix.len() <= full.len() && full[..prefix.len()] == *prefix
}

/// 严格有序追加：前缀成立且确实更长（design §3.2 的 fastForward 前提之一）。
pub fn is_strict_ordered_extension(prefix: &[String], full: &[String]) -> bool {
    prefix.len() < full.len() && is_ordered_prefix(prefix, full)
}

// ---------------------------------------------------------------------------
// 存储数据模型（design §2）
// ---------------------------------------------------------------------------

/// 关联组主表。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStore {
    pub version: u32,
    pub revision: u64,
    #[serde(default)]
    pub groups: Vec<LinkGroup>,
}

impl LinkStore {
    pub fn empty() -> Self {
        Self {
            version: LINK_STORE_VERSION,
            revision: 0,
            groups: Vec::new(),
        }
    }
}

/// 一个逻辑会话的接力组：同一逻辑会话的各账号副本归入同一组。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkGroup {
    pub id: String,
    pub variant: WbVariant,
    pub created_at: i64,
    #[serde(default)]
    pub members: Vec<LinkMember>,
    #[serde(default)]
    pub pair_bases: Vec<PairBase>,
}

/// 组内成员状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberState {
    /// 该账号当前的接力成员。
    Active,
    /// 已判定失效（正文/行缺失）但尚未被替换，保留记录不自动复活。
    Stale,
    /// 已被同账号的更新成员替换，保留记录不自动复活。
    Superseded,
}

/// 组内成员：某个账号上的某一个会话副本。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkMember {
    pub member_id: String,
    /// 工具内账号 id（可空：独立入口只有 uid）。身份判定以 uid 为准。
    #[serde(default)]
    pub account_id: Option<String>,
    pub uid: String,
    pub session_id: String,
    pub state: MemberState,
    pub linked_at: i64,
    #[serde(default)]
    pub last_synced_at: Option<i64>,
}

/// 配对基线引用（成员对无序存储，避免同一对出现两条记录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairBase {
    pub member_ids: [String; 2],
    pub baseline_ref: String,
    pub normalization_version: u32,
}

/// 基线记录本体：有序归一化行摘要 + 总摘要 + 记录数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineRecord {
    pub version: u32,
    pub baseline_ref: String,
    pub normalization_version: u32,
    pub created_at: i64,
    pub record_count: usize,
    pub total_digest: String,
    pub line_digests: Vec<String>,
}

impl BaselineRecord {
    /// 记录自洽性：版本、记录数、总摘要都能从行摘要重算出来。
    pub fn is_self_consistent(&self) -> bool {
        self.version == BASELINE_VERSION
            && self.record_count == self.line_digests.len()
            && self.total_digest == total_digest_of(&self.line_digests)
    }
}

/// 操作日志（design §2 Operation / §4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub version: u32,
    pub operation_id: String,
    /// `copy`（第一步）／`sync`（第二步预留）。
    pub kind: String,
    pub variant: WbVariant,
    /// 目标组 id：新建组时在写入前预分配，恢复时复用同一个组。
    pub group_id: String,
    pub source: OperationMember,
    pub target: OperationMember,
    /// 本次写入的正文归一化总摘要（快照），恢复时据此判断中间产物是否被改动。
    pub expected_content_digest: String,
    pub expected_record_count: usize,
    pub phase: OpPhase,
    #[serde(default)]
    pub backup: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 操作涉及的账号/会话身份。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationMember {
    #[serde(default)]
    pub account_id: Option<String>,
    pub uid: String,
    pub session_id: String,
}

/// 操作阶段：只有走到 `Completed` 才算完整成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpPhase {
    /// 已预分配目标 UUID，尚未写入任何副本。
    Prepared,
    BodyWritten,
    DbWritten,
    MappingWritten,
    LinksCommitted,
    Completed,
    /// 未写入任何内容即放弃（例如源会话已被删除），不是成功。
    Abandoned,
}

impl OpPhase {
    pub fn is_unfinished(self) -> bool {
        !matches!(self, OpPhase::Completed | OpPhase::Abandoned)
    }
}

/// 存储读取结果。
#[derive(Debug, Clone)]
pub enum StoreState {
    /// 首次使用：无主文件且无未完成痕迹。
    Missing,
    Ready(LinkStore),
    /// 保留现场并禁止写入。
    Unavailable(String),
}

/// 操作日志扫描结果：解析失败的文件必须显式上报，不能当成没有。
#[derive(Debug, Default, Clone)]
pub struct OperationScan {
    pub operations: Vec<Operation>,
    pub problems: Vec<String>,
}

/// 恢复/异常项上报。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryIssue {
    pub operation_id: String,
    pub reason: String,
    /// 重试可能成功（例如映射库暂不可用）；false 表示需要人工处理，不得盲目重放。
    pub retryable: bool,
}

/// 恢复报告。
#[derive(Debug, Default, Clone)]
pub struct RecoveryReport {
    /// 本次补齐并标记完成的操作 id。
    pub recovered: Vec<String>,
    /// 未写入任何内容即放弃的操作 id。
    pub abandoned: Vec<String>,
    /// 需要人工处理（中间产物被改动/丢失），不得盲目重放。
    pub needs_recovery: Vec<RecoveryIssue>,
}

impl RecoveryReport {
    pub fn is_clean(&self) -> bool {
        self.needs_recovery.is_empty()
    }

    /// 本次恢复是否什么都没做（宿主据此决定是否回报详情）。
    pub fn is_empty(&self) -> bool {
        self.recovered.is_empty() && self.abandoned.is_empty() && self.needs_recovery.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 锁定（design §2 / §4.1）
// ---------------------------------------------------------------------------

/// 持锁的锁文件句柄；Drop 时释放。
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// 锁失败原因：Busy = 已被其它进程持有；Unavailable = 无法建立互斥。
#[derive(Debug)]
pub enum LockError {
    Busy,
    Unavailable(String),
}

/// 档位操作锁的名字（[`LockError::message`] 的 `what`）：失败文案以它开头。
pub const VARIANT_OPS_LOCK_NAME: &str = "会话操作";
/// 档位操作锁被其它进程占用时的失败文案前缀。
///
/// 宿主（switch.rs）用它与 [`LOCK_UNAVAILABLE_MESSAGE_PREFIX`] 判断「拿不到档位锁」，
/// 不再嗅探整句错误文案；单测 `lock_failure_prefixes_match_error_messages` 保证常量
/// 与 [`LockError::message`] 的输出不漂移。
pub const LOCK_BUSY_MESSAGE_PREFIX: &str = "会话操作正被其它进程占用";
/// 档位操作锁无法建立互斥（不可用）时的失败文案前缀。
pub const LOCK_UNAVAILABLE_MESSAGE_PREFIX: &str = "会话操作不可用";

impl LockError {
    /// 锁失败文案。`what` 是被锁对象名；档位操作锁传 [`VARIANT_OPS_LOCK_NAME`]，
    /// 得到的文案分别以 [`LOCK_BUSY_MESSAGE_PREFIX`] / [`LOCK_UNAVAILABLE_MESSAGE_PREFIX`]
    /// 开头。
    pub fn message(&self, what: &str) -> String {
        match self {
            LockError::Busy => format!("{what}正被其它进程占用，请稍后重试"),
            LockError::Unavailable(reason) => format!("{what}不可用：{reason}"),
        }
    }
}

/// 以排他方式尝试锁定文件（`std::fs::File::try_lock`，1.89+ 稳定）。
///
/// 明确被占用才返回 Busy；其余失败一律 Unavailable——无互斥保证时不得继续写。
pub fn try_lock_file(path: &Path) -> Result<FileLock, LockError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| LockError::Unavailable(error.to_string()))?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| LockError::Unavailable(error.to_string()))?;
    match file.try_lock() {
        Ok(()) => Ok(FileLock { file }),
        Err(TryLockError::WouldBlock) => Err(LockError::Busy),
        Err(TryLockError::Error(error)) => Err(LockError::Unavailable(error.to_string())),
    }
}

/// 档位操作锁：覆盖一次完整的会话操作（查询关联 → 写副本 → 提交关联）。
///
/// 并发请求与中断重试靠它串行化；被占用时直接拒绝，不排队、不绕过。
pub fn try_acquire_variant_ops_lock(
    paths: &SessionPaths,
    variant: WbVariant,
) -> Result<FileLock, String> {
    try_lock_file(&paths.variant_ops_lock_file(variant))
        .map_err(|error| error.message(VARIANT_OPS_LOCK_NAME))
}

fn acquire_link_store_lock(paths: &SessionPaths) -> Result<FileLock, String> {
    let path = paths.link_store_lock_file();
    for _ in 0..STORE_LOCK_RETRY {
        match try_lock_file(&path) {
            Ok(lock) => return Ok(lock),
            Err(LockError::Busy) => std::thread::sleep(STORE_LOCK_RETRY_INTERVAL),
            Err(LockError::Unavailable(reason)) => {
                return Err(format!("关联存储锁不可用：{reason}"));
            }
        }
    }
    Err("关联存储正被其它写入占用，请稍后重试".to_string())
}

// ---------------------------------------------------------------------------
// 关联存储读写
// ---------------------------------------------------------------------------

/// 目录下是否存在任意 `*.json` 条目（目录读不了时按「没有」处理）。
fn has_json_entries(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
}

/// 是否存在未完成的残留痕迹（主文件缺失时据此拒绝初始化）。
///
/// 基线文件同样算痕迹：基线代表既有的关联关系，主文件缺失只能是异常现场，
/// 不能当成首次使用重建空表（design §2）。
fn has_unfinished_traces(paths: &SessionPaths) -> bool {
    if has_json_entries(&paths.baselines_dir()) {
        return true;
    }
    let dir = paths.operations_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            // 读不了的残片同样算痕迹：不能当成空表覆盖。
            return true;
        };
        match serde_json::from_str::<Operation>(&text) {
            Ok(operation) => {
                if operation.phase.is_unfinished() {
                    return true;
                }
            }
            Err(_) => return true,
        }
    }
    false
}

/// 读取关联主表；区分首次缺失与不可用。
pub fn load_store(paths: &SessionPaths) -> StoreState {
    let file = paths.session_links_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if has_unfinished_traces(paths) {
                return StoreState::Unavailable(
                    "关联存储主文件缺失但存在未完成的痕迹（操作记录或基线），已保留现场"
                        .to_string(),
                );
            }
            return StoreState::Missing;
        }
        Err(error) => return StoreState::Unavailable(format!("关联存储不可读：{error}")),
    };
    let store = match serde_json::from_str::<LinkStore>(&text) {
        Ok(store) => store,
        Err(_) => {
            return StoreState::Unavailable("关联存储内容损坏，已保留原文件".to_string());
        }
    };
    if store.version != LINK_STORE_VERSION {
        return StoreState::Unavailable(format!(
            "关联存储版本 {} 不受支持（当前支持 {}），已保留原文件",
            store.version, LINK_STORE_VERSION
        ));
    }
    if let Err(reason) = validate_store(&store) {
        return StoreState::Unavailable(format!("关联存储不满足不变量：{reason}"));
    }
    StoreState::Ready(store)
}

/// 不变量校验（写入前与读取后都执行）：
/// - 组 id 唯一；
/// - 同一 (variant, uid, sessionId) 只属于一个组；
/// - 每组每个账号至多一个 active 成员；
/// - 配对基线双方都在组内且不重复。
pub fn validate_store(store: &LinkStore) -> Result<(), String> {
    let mut group_ids = std::collections::HashSet::new();
    let mut identities = std::collections::HashSet::new();
    for group in &store.groups {
        if !group_ids.insert(group.id.as_str()) {
            return Err(format!("组 id 重复：{}", group.id));
        }
        let mut member_ids = std::collections::HashSet::new();
        let mut active_by_uid = std::collections::HashSet::new();
        for member in &group.members {
            if !member_ids.insert(member.member_id.as_str()) {
                return Err(format!("成员 id 重复：{}", member.member_id));
            }
            let identity = (
                group.variant.as_str(),
                member.uid.as_str(),
                member.session_id.as_str(),
            );
            if !identities.insert(identity) {
                return Err(format!("会话 {} 同时属于多个关联组", member.session_id));
            }
            if member.state == MemberState::Active && !active_by_uid.insert(member.uid.as_str()) {
                return Err(format!("账号 {} 在组内出现多个 active 成员", member.uid));
            }
        }
        let mut pairs = std::collections::HashSet::new();
        for pair in &group.pair_bases {
            if !pairs.insert(pair_key(&pair.member_ids[0], &pair.member_ids[1])) {
                return Err(format!("成员对重复存储基线：{}", pair.baseline_ref));
            }
            for member_id in pair.member_ids.iter() {
                if !member_ids.contains(member_id.as_str()) {
                    return Err(format!("基线引用了组内不存在的成员：{member_id}"));
                }
            }
        }
    }
    Ok(())
}

/// 规范化的成员对 key（无序成员对唯一）。
pub fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn pair_matches(pair: &PairBase, key: &(String, String)) -> bool {
    pair.member_ids[0] == key.0 && pair.member_ids[1] == key.1
}

/// 关联存储读改写：在存储锁内重读、修改、校验、原子写回并递增 revision。
///
/// 存储不可用（损坏/未知版本/权限失败）时直接失败：不得降级成空表覆盖。
pub fn with_link_store_write<T>(
    paths: &SessionPaths,
    mutate: impl FnOnce(&mut LinkStore) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = acquire_link_store_lock(paths)?;
    let (mut store, existed) = match load_store(paths) {
        StoreState::Missing => (LinkStore::empty(), false),
        StoreState::Ready(store) => (store, true),
        StoreState::Unavailable(reason) => return Err(reason),
    };
    let outcome = mutate(&mut store)?;
    validate_store(&store).map_err(|reason| format!("拒绝写入不一致的关联存储：{reason}"))?;
    store.version = LINK_STORE_VERSION;
    store.revision = store.revision.saturating_add(1);
    let content = serde_json::to_string_pretty(&store).map_err(|error| error.to_string())?;
    if let Some(parent) = paths.session_links_file().parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    atomic_write(&paths.session_links_file(), &content).map_err(|error| {
        if !existed {
            format!("关联存储首次写入失败：{error}")
        } else {
            format!("关联存储写入失败：{error}")
        }
    })?;
    Ok(outcome)
}

/// 按 (variant, uid, sessionId) 查找所属组（身份唯一归组）。
pub fn find_group_for_identity<'a>(
    store: &'a LinkStore,
    variant: WbVariant,
    uid: &str,
    session_id: &str,
) -> Option<&'a LinkGroup> {
    store.groups.iter().find(|group| {
        group.variant == variant
            && group
                .members
                .iter()
                .any(|member| member.uid == uid && member.session_id == session_id)
    })
}

/// 组内某账号的 active 成员。
pub fn active_member_for<'a>(group: &'a LinkGroup, uid: &str) -> Option<&'a LinkMember> {
    group
        .members
        .iter()
        .find(|member| member.uid == uid && member.state == MemberState::Active)
}

/// 组内按 (uid, sessionId) 定位成员（任意状态）。
pub fn find_member<'a>(
    group: &'a LinkGroup,
    uid: &str,
    session_id: &str,
) -> Option<&'a LinkMember> {
    group
        .members
        .iter()
        .find(|member| member.uid == uid && member.session_id == session_id)
}

/// 加入一个 active 成员：同账号已有的 active 成员显式转 superseded。
///
/// 旧成员保留记录且不再有效；正文之后被恢复也不会自动复活（design §2）。
pub fn add_active_member(group: &mut LinkGroup, member: LinkMember) {
    for existing in group.members.iter_mut() {
        if existing.uid == member.uid && existing.state == MemberState::Active {
            existing.state = MemberState::Superseded;
        }
    }
    group.members.push(member);
}

/// 显式改变成员状态（失效标记/恢复记录用；不做自动复活）。
pub fn set_member_state(group: &mut LinkGroup, member_id: &str, state: MemberState) -> bool {
    match group
        .members
        .iter_mut()
        .find(|member| member.member_id == member_id)
    {
        Some(member) => {
            member.state = state;
            true
        }
        None => false,
    }
}

/// 定位成员对的基线引用（无序）。
pub fn find_pair_base<'a>(
    group: &'a LinkGroup,
    member_a: &str,
    member_b: &str,
) -> Option<&'a PairBase> {
    let key = pair_key(member_a, member_b);
    group
        .pair_bases
        .iter()
        .find(|pair| pair_matches(pair, &key))
}

/// 定向写入成员对的基线：只影响这一对，不触碰同组成员的其它配对。
pub fn set_pair_base(
    group: &mut LinkGroup,
    member_a: &str,
    member_b: &str,
    baseline_ref: &str,
    normalization_version: u32,
) {
    let key = pair_key(member_a, member_b);
    let entry = PairBase {
        member_ids: [key.0.clone(), key.1.clone()],
        baseline_ref: baseline_ref.to_string(),
        normalization_version,
    };
    match group
        .pair_bases
        .iter_mut()
        .find(|pair| pair_matches(pair, &key))
    {
        Some(existing) => *existing = entry,
        None => group.pair_bases.push(entry),
    }
}

// ---------------------------------------------------------------------------
// 基线文件
// ---------------------------------------------------------------------------

fn baseline_file(paths: &SessionPaths, baseline_ref: &str) -> PathBuf {
    paths.baselines_dir().join(format!("{baseline_ref}.json"))
}

/// 写入配对基线文件（内容自洽；同一 ref 覆盖写等价）。
pub fn save_baseline(
    paths: &SessionPaths,
    baseline_ref: &str,
    normalized: &NormalizedContent,
) -> Result<BaselineRecord, String> {
    let record = BaselineRecord {
        version: BASELINE_VERSION,
        baseline_ref: baseline_ref.to_string(),
        normalization_version: NORMALIZATION_VERSION,
        created_at: now_ms(),
        record_count: normalized.record_count,
        total_digest: normalized.total_digest.clone(),
        line_digests: normalized.line_digests.clone(),
    };
    if !record.is_self_consistent() {
        return Err("基线内容不自洽，拒绝写入".to_string());
    }
    std::fs::create_dir_all(paths.baselines_dir()).map_err(|error| error.to_string())?;
    let content = serde_json::to_string_pretty(&record).map_err(|error| error.to_string())?;
    atomic_write(&baseline_file(paths, baseline_ref), &content)
        .map_err(|error| error.to_string())?;
    Ok(record)
}

/// 读取配对基线；缺失/损坏/版本不符/归一化版本不符一律视为不可验证（None）。
pub fn load_baseline(paths: &SessionPaths, baseline_ref: &str) -> Option<BaselineRecord> {
    let text = std::fs::read_to_string(baseline_file(paths, baseline_ref)).ok()?;
    let record: BaselineRecord = serde_json::from_str(&text).ok()?;
    if !record.is_self_consistent() || record.normalization_version != NORMALIZATION_VERSION {
        return None;
    }
    Some(record)
}

/// 由来源内容向新成员继承配对基线：要求来源内容有序前缀包含该基线，否则不推断。
///
/// 例：A/B 已有基线 X，来源 A 的当前内容以 X 为前缀，则新成员 C 可建立 B/C = X 的
/// 共同基线（C 的内容 = A 的当前内容）。无法证明包含关系时不建立。
pub fn inheritable_baseline(
    paths: &SessionPaths,
    pair: &PairBase,
    source_line_digests: &[String],
) -> Option<BaselineRecord> {
    if pair.normalization_version != NORMALIZATION_VERSION {
        return None;
    }
    let record = load_baseline(paths, &pair.baseline_ref)?;
    if !is_ordered_prefix(&record.line_digests, source_line_digests) {
        return None;
    }
    Some(record)
}

// ---------------------------------------------------------------------------
// 操作日志
// ---------------------------------------------------------------------------

fn operation_file(paths: &SessionPaths, operation_id: &str) -> PathBuf {
    paths.operations_dir().join(format!("{operation_id}.json"))
}

/// 原子写入操作日志。
pub fn save_operation(paths: &SessionPaths, operation: &Operation) -> Result<(), String> {
    std::fs::create_dir_all(paths.operations_dir()).map_err(|error| error.to_string())?;
    let content = serde_json::to_string_pretty(operation).map_err(|error| error.to_string())?;
    atomic_write(&operation_file(paths, &operation.operation_id), &content)
        .map_err(|error| format!("操作记录写入失败：{error}"))
}

/// 扫描全部操作日志（不区分档位）；解析失败的文件作为问题上报。
pub fn scan_operations(paths: &SessionPaths) -> OperationScan {
    let mut scan = OperationScan::default();
    let Ok(entries) = std::fs::read_dir(paths.operations_dir()) else {
        return scan;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        match std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<Operation>(&text).map_err(|error| error.to_string())
            }) {
            Ok(operation) => scan.operations.push(operation),
            Err(reason) => scan.problems.push(format!("{name}: {reason}")),
        }
    }
    scan.operations
        .sort_by_key(|operation| operation.created_at);
    scan
}

/// 收集某档位未完成的操作。
pub fn pending_operations(paths: &SessionPaths, variant: WbVariant) -> Vec<Operation> {
    scan_operations(paths)
        .operations
        .into_iter()
        .filter(|operation| operation.variant == variant && operation.phase.is_unfinished())
        .collect()
}

/// 查找与本次请求同一（来源会话 → 目标账号）的未完成操作，避免重复副本。
pub fn find_pending_operation<'a>(
    operations: &'a [Operation],
    source_uid: &str,
    source_session_id: &str,
    target_uid: &str,
) -> Option<&'a Operation> {
    operations.iter().find(|operation| {
        operation.phase.is_unfinished()
            && operation.source.uid == source_uid
            && operation.source.session_id == source_session_id
            && operation.target.uid == target_uid
    })
}

/// 清理已完成/已放弃的历史操作日志，每档位保留最近 `keep` 条。
pub fn prune_operations(paths: &SessionPaths, variant: WbVariant, keep: usize) -> usize {
    let scan = scan_operations(paths);
    let mut finished: Vec<&Operation> = scan
        .operations
        .iter()
        .filter(|operation| operation.variant == variant && !operation.phase.is_unfinished())
        .collect();
    if finished.len() <= keep {
        return 0;
    }
    finished.sort_by_key(|operation| std::cmp::Reverse(operation.updated_at));
    let mut removed = 0usize;
    for operation in finished.into_iter().skip(keep) {
        if std::fs::remove_file(operation_file(paths, &operation.operation_id)).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_link_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_paths(dir: &TempDir) -> SessionPaths {
        SessionPaths {
            store_root: dir.path().join("store"),
            data_root: dir.path().join("data"),
            auth_file: dir.path().join("auth.info"),
        }
    }

    fn member(uid: &str, session_id: &str, state: MemberState) -> LinkMember {
        LinkMember {
            member_id: format!("m-{uid}-{session_id}"),
            account_id: None,
            uid: uid.to_string(),
            session_id: session_id.to_string(),
            state,
            linked_at: 1,
            last_synced_at: None,
        }
    }

    fn group_with_members(id: &str, members: Vec<LinkMember>) -> LinkGroup {
        LinkGroup {
            id: id.to_string(),
            variant: WbVariant::Cn,
            created_at: 1,
            members,
            pair_bases: Vec::new(),
        }
    }

    fn operation(id: &str, phase: OpPhase, target_uid: &str, target_session: &str) -> Operation {
        Operation {
            version: OPERATION_VERSION,
            operation_id: id.to_string(),
            kind: "copy".to_string(),
            variant: WbVariant::Cn,
            group_id: "g-1".to_string(),
            source: OperationMember {
                account_id: None,
                uid: "uid-a".to_string(),
                session_id: "sess-1".to_string(),
            },
            target: OperationMember {
                account_id: None,
                uid: target_uid.to_string(),
                session_id: target_session.to_string(),
            },
            expected_content_digest: "digest".to_string(),
            expected_record_count: 1,
            phase,
            backup: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn sample_body(cid: &str) -> String {
        format!(
            "{}\n{}\n",
            r#"{"type":"user","sessionId":"CID","text":"你好"}"#.replace("CID", cid),
            r#"{"type":"assistant","sessionId":"CID","text":"hi"}"#.replace("CID", cid)
        )
    }

    // -----------------------------------------------------------------------
    // 内容版本
    // -----------------------------------------------------------------------

    /// 归一化只替换本副本自己的 sessionId；其它 sessionId 保留 → 产生保守差异。
    #[test]
    fn normalize_replaces_only_own_session_id() {
        let other = "11111111-2222-3333-4444-555555555555";
        let own = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let left = format!(
            "{}\n",
            format_args!(r#"{{"sessionId":"{own}","parentSessionId":"{other}"}}"#)
        );
        let right = format!(
            "{}\n",
            format_args!(
                r#"{{"sessionId":"{own}","parentSessionId":"99999999-8888-7777-6666-555555555555"}}"#
            )
        );
        let left = normalize_jsonl(&left, own).unwrap();
        let right = normalize_jsonl(&right, own).unwrap();
        assert_ne!(
            left.total_digest, right.total_digest,
            "第二个 sessionId 必须保留并参与摘要"
        );

        // 只有本副本 id 不同 → 归一化后完全一致。
        let copy = format!(
            "{}\n",
            format_args!(r#"{{"sessionId":"{other}","text":"x"}}"#)
        );
        let original = format!(
            "{}\n",
            format_args!(r#"{{"sessionId":"{own}","text":"x"}}"#)
        );
        let copy = normalize_jsonl(&copy, other).unwrap();
        let original = normalize_jsonl(&original, own).unwrap();
        assert_eq!(copy.total_digest, original.total_digest);
        assert_eq!(copy.line_digests, original.line_digests);
    }

    /// 顺序与重复次数参与摘要，不能按 id 去重。
    #[test]
    fn normalize_keeps_order_and_duplicates() {
        let own = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let line_a = r#"{"sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","text":"a"}"#;
        let line_b = r#"{"sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","text":"b"}"#;

        let ab = normalize_jsonl(&format!("{line_a}\n{line_b}\n"), own).unwrap();
        let ba = normalize_jsonl(&format!("{line_b}\n{line_a}\n"), own).unwrap();
        assert_ne!(ab.total_digest, ba.total_digest, "重排必须产生不同摘要");

        let duplicated = normalize_jsonl(&format!("{line_a}\n{line_a}\n{line_b}\n"), own).unwrap();
        assert_eq!(duplicated.record_count, 3);
        assert_ne!(ab.total_digest, duplicated.total_digest);
        // 重复行不去重：逐行摘要允许出现重复项。
        assert_eq!(duplicated.line_digests[0], duplicated.line_digests[1]);
    }

    /// 键序变化是保守差异（本工具不重排用户内容）。
    #[test]
    fn normalize_treats_key_order_change_as_difference() {
        let own = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let a = normalize_jsonl(&format!(r#"{{"sessionId":"{own}","text":"x"}}"#), own).unwrap();
        let b = normalize_jsonl(&format!(r#"{{"text":"x","sessionId":"{own}"}}"#), own).unwrap();
        assert_ne!(a.total_digest, b.total_digest, "键序变化应保守视为差异");
    }

    /// 空 / 非法 / 截断一律 Unavailable；文件缺失才是 Missing。
    #[test]
    fn content_state_rejects_empty_invalid_and_truncated() {
        let dir = TempDir::new("content");
        let own = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let missing = dir.path().join("missing.jsonl");
        assert_eq!(read_content_snapshot(&missing, own), ContentState::Missing);

        for (name, text) in [
            ("empty", ""),
            ("blank", "   \n"),
            ("not-json", "hello\n"),
            ("truncated", "{\"sessionId\":\"x\"\n"),
            ("blank-line", "{\"a\":1}\n\n{\"b\":2}\n"),
        ] {
            let path = dir.path().join(format!("{name}.jsonl"));
            std::fs::write(&path, text).unwrap();
            assert!(
                matches!(
                    read_content_snapshot(&path, own),
                    ContentState::Unavailable(_)
                ),
                "{name} 必须判为不可验证"
            );
        }

        let ok = dir.path().join("ok.jsonl");
        std::fs::write(&ok, sample_body(own)).unwrap();
        assert!(matches!(
            read_content_snapshot(&ok, own),
            ContentState::Ready(_)
        ));

        // 末尾多几个换行不算截断；中间空行仍算异常。
        let trailing = dir.path().join("trailing.jsonl");
        std::fs::write(&trailing, format!("{}\n\n", sample_body(own).trim_end())).unwrap();
        match read_content_snapshot(&trailing, own) {
            ContentState::Ready(snapshot) => assert_eq!(snapshot.normalized.record_count, 2),
            other => panic!("末尾换行应容忍，实际 {other:?}"),
        }
    }

    /// 全文摘要覆盖空白变化；归一化摘要不受空白影响。
    #[test]
    fn full_digest_covers_raw_bytes_while_normalized_ignores_spacing() {
        let own = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let base = format!(r#"{{"sessionId":"{own}","text":"x"}}"#);
        let padded = format!("{{ \"sessionId\":\"{own}\", \"text\":\"x\" }}");
        let a = normalize_jsonl(&format!("{base}\n"), own).unwrap();
        let b = normalize_jsonl(&format!("{padded}\n"), own).unwrap();
        assert_ne!(a.total_digest, b.total_digest);
        assert_ne!(
            full_digest_of(base.as_bytes()),
            full_digest_of(padded.as_bytes())
        );
        assert_eq!(
            full_digest_of(base.as_bytes()),
            full_digest_of(base.as_bytes())
        );
    }

    /// 摘要算法与归一化版本固定：值变化必须显式提升 normalizationVersion。
    #[test]
    fn digest_is_stable_for_fixed_content() {
        let normalized = normalize_jsonl("{\"a\":1}\n", "sess").unwrap();
        assert_eq!(normalized.record_count, 1);
        assert_eq!(NORMALIZATION_VERSION, 1);
        assert_eq!(
            normalized.line_digests[0],
            "37b3e6066c98ad57f90d4c840524b677383e4bc21f1e562e1426b753eb1e08f8"
        );
        assert_eq!(
            normalized.total_digest,
            "3e1fed6dd7560c99d45e5561fb26f645ef539a8fe7ce11b2df67c5b0ed9487d6"
        );
        // 总摘要可由行摘要重算，篡改后的基线因此自曝不一致。
        assert_eq!(
            total_digest_of(&normalized.line_digests),
            normalized.total_digest
        );
        assert_ne!(
            total_digest_of(&["00".to_string()]),
            normalized.total_digest
        );
        assert_eq!(
            full_digest_of(b"{\"a\":1}\n"),
            "e346432021b04179518d9614f3560ccd71354a4ee101ddcb893d6959a9d6301c"
        );
    }

    #[test]
    fn ordered_prefix_helpers() {
        let base = vec!["a".to_string(), "b".to_string()];
        let extended = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let reordered = vec!["b".to_string(), "a".to_string()];
        assert!(is_ordered_prefix(&base, &extended));
        assert!(is_ordered_prefix(&base, &base));
        assert!(!is_ordered_prefix(&extended, &base));
        assert!(!is_ordered_prefix(&reordered, &extended));
        assert!(is_strict_ordered_extension(&base, &extended));
        assert!(!is_strict_ordered_extension(&base, &base));
    }

    // -----------------------------------------------------------------------
    // 关联存储
    // -----------------------------------------------------------------------

    #[test]
    fn store_initializes_missing_then_round_trips_with_revision() {
        let dir = TempDir::new("store-round-trip");
        let paths = temp_paths(&dir);
        assert!(matches!(load_store(&paths), StoreState::Missing));

        let revision = with_link_store_write(&paths, |store| {
            store.groups.push(group_with_members(
                "g-1",
                vec![member("uid-a", "sess-1", MemberState::Active)],
            ));
            Ok(store.revision)
        })
        .unwrap();
        assert_eq!(revision, 0, "首次写入前 revision 为 0");

        match load_store(&paths) {
            StoreState::Ready(store) => {
                assert_eq!(store.version, LINK_STORE_VERSION);
                assert_eq!(store.revision, 1);
                assert_eq!(store.groups.len(), 1);
            }
            other => panic!("期望 Ready，实际 {other:?}"),
        }
    }

    #[test]
    fn store_corrupt_or_unknown_version_is_unavailable_and_preserved() {
        let dir = TempDir::new("store-corrupt");
        let paths = temp_paths(&dir);
        std::fs::create_dir_all(&paths.store_root).unwrap();

        for (name, content) in [
            ("corrupt", "not-json".to_string()),
            (
                "unknown-version",
                r#"{"version":99,"revision":1,"groups":[]}"#.to_string(),
            ),
        ] {
            std::fs::write(paths.session_links_file(), &content).unwrap();
            assert!(
                matches!(load_store(&paths), StoreState::Unavailable(_)),
                "{name} 必须判为不可用"
            );
            let write = with_link_store_write(&paths, |store| {
                store.groups.push(group_with_members("g-x", vec![]));
                Ok(())
            });
            assert!(write.is_err(), "{name} 时必须禁写");
            assert_eq!(
                std::fs::read_to_string(paths.session_links_file()).unwrap(),
                content,
                "{name}：必须保留现场，不得覆盖"
            );
        }
    }

    /// 权限错误（读不了主文件）→ 不可用且禁写。
    #[cfg(unix)]
    #[test]
    fn store_permission_denied_is_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("store-perm");
        let paths = temp_paths(&dir);
        std::fs::create_dir_all(&paths.store_root).unwrap();
        std::fs::write(paths.session_links_file(), json_store(vec![])).unwrap();
        std::fs::set_permissions(
            paths.session_links_file(),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        if std::fs::read_to_string(paths.session_links_file()).is_ok() {
            // root / 特殊 ACL 环境读得到，跳过（不误报）。
            std::fs::set_permissions(
                paths.session_links_file(),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            return;
        }
        assert!(matches!(load_store(&paths), StoreState::Unavailable(_)));
        assert!(with_link_store_write(&paths, |_| Ok(())).is_err());
        std::fs::set_permissions(
            paths.session_links_file(),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }

    /// 主文件缺失但残留未完成操作 → 不可用（不得当成空表初始化）。
    #[test]
    fn store_missing_file_with_unfinished_operation_is_unavailable() {
        let dir = TempDir::new("store-residual");
        let paths = temp_paths(&dir);
        save_operation(
            &paths,
            &operation("op-1", OpPhase::BodyWritten, "uid-b", "sess-b"),
        )
        .unwrap();
        assert!(matches!(load_store(&paths), StoreState::Unavailable(_)));

        // 已完成的痕迹不算残留：可以按首次初始化。
        save_operation(
            &paths,
            &operation("op-2", OpPhase::Completed, "uid-b", "sess-b"),
        )
        .unwrap();
        std::fs::remove_file(paths.operations_dir().join("op-1.json")).unwrap();
        assert!(matches!(load_store(&paths), StoreState::Missing));
    }

    /// 主文件缺失但残留基线文件 → 不可用（不得当成空表初始化并覆盖现场）。
    #[test]
    fn store_missing_file_with_orphan_baseline_is_unavailable() {
        let dir = TempDir::new("store-baseline-residual");
        let paths = temp_paths(&dir);
        let normalized = normalize_jsonl("{\"a\":1}\n", "sess").unwrap();
        save_baseline(&paths, "base-orphan", &normalized).unwrap();

        assert!(matches!(load_store(&paths), StoreState::Unavailable(_)));
        assert!(with_link_store_write(&paths, |_| Ok(())).is_err());
        assert!(!paths.session_links_file().exists(), "不得重建空表覆盖现场");
        assert!(
            paths.baselines_dir().join("base-orphan.json").exists(),
            "必须保留基线现场"
        );
    }

    #[test]
    fn store_rejects_invariant_violations() {
        let dir = TempDir::new("store-invariant");
        let paths = temp_paths(&dir);
        std::fs::create_dir_all(&paths.store_root).unwrap();

        // 同账号两个 active 成员。
        let two_active = json_store(vec![group_with_members(
            "g-1",
            vec![
                member("uid-b", "sess-b1", MemberState::Active),
                member("uid-b", "sess-b2", MemberState::Active),
            ],
        )]);
        std::fs::write(paths.session_links_file(), two_active).unwrap();
        assert!(matches!(load_store(&paths), StoreState::Unavailable(_)));

        // 同一会话身份出现在两个组。
        let duplicated = json_store(vec![
            group_with_members("g-1", vec![member("uid-a", "sess-1", MemberState::Active)]),
            group_with_members("g-2", vec![member("uid-a", "sess-1", MemberState::Active)]),
        ]);
        std::fs::write(paths.session_links_file(), duplicated).unwrap();
        assert!(matches!(load_store(&paths), StoreState::Unavailable(_)));
    }

    fn json_store(groups: Vec<LinkGroup>) -> String {
        let store = LinkStore {
            version: LINK_STORE_VERSION,
            revision: 1,
            groups,
        };
        serde_json::to_string_pretty(&store).unwrap()
    }

    #[test]
    fn add_active_member_supersedes_previous_active_only() {
        let mut group =
            group_with_members("g-1", vec![member("uid-b", "sess-b1", MemberState::Active)]);
        add_active_member(&mut group, member("uid-b", "sess-b2", MemberState::Active));
        assert_eq!(
            group
                .members
                .iter()
                .filter(|m| m.uid == "uid-b" && m.state == MemberState::Active)
                .count(),
            1
        );
        assert_eq!(
            group
                .members
                .iter()
                .find(|m| m.session_id == "sess-b1")
                .unwrap()
                .state,
            MemberState::Superseded
        );

        // 不自动复活：显式置回 active 才恢复。
        assert!(set_member_state(
            &mut group,
            "m-uid-b-sess-b1",
            MemberState::Active
        ));
        assert_eq!(
            group
                .members
                .iter()
                .filter(|m| m.state == MemberState::Active)
                .count(),
            2
        );
    }

    #[test]
    fn pair_bases_are_per_member_pair_and_updates_are_directed() {
        let mut group = group_with_members(
            "g-1",
            vec![
                member("uid-a", "sess-1", MemberState::Active),
                member("uid-b", "sess-b", MemberState::Active),
                member("uid-c", "sess-c", MemberState::Active),
            ],
        );
        set_pair_base(&mut group, "m-uid-a-sess-1", "m-uid-b-sess-b", "base-ab", 1);
        set_pair_base(&mut group, "m-uid-a-sess-1", "m-uid-c-sess-c", "base-ac", 1);
        assert_eq!(group.pair_bases.len(), 2);

        // 无序查找：任一方向都能定位同一对。
        assert_eq!(
            find_pair_base(&group, "m-uid-b-sess-b", "m-uid-a-sess-1")
                .unwrap()
                .baseline_ref,
            "base-ab"
        );

        // 定向更新 A/B 不触碰 A/C。
        set_pair_base(
            &mut group,
            "m-uid-a-sess-1",
            "m-uid-b-sess-b",
            "base-ab2",
            1,
        );
        assert_eq!(
            find_pair_base(&group, "m-uid-a-sess-1", "m-uid-b-sess-b")
                .unwrap()
                .baseline_ref,
            "base-ab2"
        );
        assert_eq!(
            find_pair_base(&group, "m-uid-a-sess-1", "m-uid-c-sess-c")
                .unwrap()
                .baseline_ref,
            "base-ac"
        );
        assert_eq!(group.pair_bases.len(), 2);
    }

    #[test]
    fn baseline_inheritance_requires_verified_prefix() {
        let dir = TempDir::new("baseline-inherit");
        let paths = temp_paths(&dir);
        let own = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let body = sample_body(own);
        let full = normalize_jsonl(&body, own).unwrap();

        let record = save_baseline(&paths, "base-ab", &full).unwrap();
        assert!(record.is_self_consistent());
        assert_eq!(
            load_baseline(&paths, "base-ab").unwrap().record_count,
            full.record_count
        );

        let pair = PairBase {
            member_ids: ["m-a".to_string(), "m-b".to_string()],
            baseline_ref: "base-ab".to_string(),
            normalization_version: NORMALIZATION_VERSION,
        };
        // 来源内容包含该基线（等价）→ 可继承。
        assert!(inheritable_baseline(&paths, &pair, &full.line_digests).is_some());

        // 来源内容以该基线为前缀（有序追加）→ 可继承。
        let mut extended = full.line_digests.clone();
        extended.push("later".to_string());
        assert!(inheritable_baseline(&paths, &pair, &extended).is_some());

        // 来源内容短于基线 / 顺序不同 → 不可证明，不继承。
        assert!(inheritable_baseline(&paths, &pair, &full.line_digests[..1]).is_none());
        let mut reordered = full.line_digests.clone();
        reordered.reverse();
        if reordered != full.line_digests {
            assert!(inheritable_baseline(&paths, &pair, &reordered).is_none());
        }

        // 基线文件不存在 → 不可验证（Unknown），不继承。
        let missing_pair = PairBase {
            member_ids: ["m-a".to_string(), "m-c".to_string()],
            baseline_ref: "base-missing".to_string(),
            normalization_version: NORMALIZATION_VERSION,
        };
        assert!(inheritable_baseline(&paths, &missing_pair, &full.line_digests).is_none());

        // 基线文件损坏 / 版本不符 → 不可验证。
        std::fs::write(paths.baselines_dir().join("base-ab.json"), "not-json").unwrap();
        assert!(inheritable_baseline(&paths, &pair, &full.line_digests).is_none());
    }

    // -----------------------------------------------------------------------
    // 操作日志与锁
    // -----------------------------------------------------------------------

    #[test]
    fn operations_round_trip_filter_by_variant_and_prune() {
        let dir = TempDir::new("ops");
        let paths = temp_paths(&dir);
        save_operation(
            &paths,
            &operation("op-cn", OpPhase::Prepared, "uid-b", "s1"),
        )
        .unwrap();
        let mut ai = operation("op-ai", OpPhase::Prepared, "uid-b", "s2");
        ai.variant = WbVariant::Ai;
        save_operation(&paths, &ai).unwrap();
        save_operation(
            &paths,
            &operation("op-done", OpPhase::Completed, "uid-b", "s3"),
        )
        .unwrap();

        let pending = pending_operations(&paths, WbVariant::Cn);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, "op-cn");
        assert_eq!(pending_operations(&paths, WbVariant::Ai).len(), 1);

        assert!(find_pending_operation(&pending, "uid-a", "sess-1", "uid-b").is_some());
        assert!(find_pending_operation(&pending, "uid-a", "sess-1", "uid-c").is_none());

        // 损坏的操作文件必须显式上报，不能当成没有。
        std::fs::write(paths.operations_dir().join("broken.json"), "not-json").unwrap();
        assert_eq!(scan_operations(&paths).problems.len(), 1);

        // 清理只删已完成的历史，保留未完成。
        for index in 0..5 {
            let mut done = operation(&format!("done-{index}"), OpPhase::Completed, "uid-b", "s9");
            done.updated_at = 100 + index;
            save_operation(&paths, &done).unwrap();
        }
        assert!(prune_operations(&paths, WbVariant::Cn, 2) >= 4);
        assert!(paths.operations_dir().join("op-cn.json").exists());
    }

    /// 锁失败文案前缀常量与 [`LockError::message`] 的输出一致：宿主据此判断锁失败，
    /// 不再嗅探整句错误文案。
    #[test]
    fn lock_failure_prefixes_match_error_messages() {
        let busy = LockError::Busy.message(VARIANT_OPS_LOCK_NAME);
        assert!(busy.starts_with(LOCK_BUSY_MESSAGE_PREFIX), "{busy}");
        let unavailable =
            LockError::Unavailable("磁盘异常".to_string()).message(VARIANT_OPS_LOCK_NAME);
        assert!(
            unavailable.starts_with(LOCK_UNAVAILABLE_MESSAGE_PREFIX),
            "{unavailable}"
        );
        // 其它锁对象的文案不得被误判成档位锁失败。
        assert!(!LockError::Busy
            .message("关联存储")
            .starts_with(LOCK_BUSY_MESSAGE_PREFIX));
    }

    #[test]
    fn second_lock_handle_is_busy_until_first_releases() {
        let dir = TempDir::new("lock-fd");
        let path = dir.path().join("a.lock");
        let held = try_lock_file(&path).unwrap();
        match try_lock_file(&path) {
            Err(LockError::Busy) => {}
            Err(LockError::Unavailable(reason)) => panic!("期望 Busy，实际 Unavailable: {reason}"),
            Ok(_) => panic!("第二个句柄不应拿到同一把锁"),
        }
        drop(held);
        assert!(try_lock_file(&path).is_ok());
    }

    /// 子进程助手：设置 WB_SWITCH_TEST_HOLD_LOCK_MS 时持锁并写就绪文件。
    #[test]
    fn lock_holder_child_process() {
        let Ok(hold_ms) = std::env::var("WB_SWITCH_TEST_HOLD_LOCK_MS") else {
            return;
        };
        let lock_path = std::env::var("WB_SWITCH_TEST_LOCK_PATH").expect("lock path");
        let ready_path = std::env::var("WB_SWITCH_TEST_READY_PATH").expect("ready path");
        let _lock = try_lock_file(Path::new(&lock_path)).expect("child must acquire lock");
        std::fs::write(&ready_path, b"ready").unwrap();
        let hold_ms: u64 = hold_ms.parse().unwrap_or(2000);
        std::thread::sleep(Duration::from_millis(hold_ms));
    }

    /// 跨进程互斥：另一个进程持锁时本进程只能得到 Busy。
    #[test]
    fn cross_process_lock_is_busy() {
        let dir = TempDir::new("lock-proc");
        let lock_path = dir.path().join("cross.lock");
        let ready_path = dir.path().join("ready");
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Ok(mut child) = std::process::Command::new(exe)
            .args([
                "--exact",
                "modules::session_link::tests::lock_holder_child_process",
                "--nocapture",
            ])
            .env("WB_SWITCH_TEST_HOLD_LOCK_MS", "2000")
            .env("WB_SWITCH_TEST_LOCK_PATH", &lock_path)
            .env("WB_SWITCH_TEST_READY_PATH", &ready_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };

        let mut waited_ms = 0u64;
        while !ready_path.exists() && waited_ms < 5000 {
            std::thread::sleep(Duration::from_millis(20));
            waited_ms += 20;
        }
        if !ready_path.exists() {
            // 子进程没起来（受限环境）：不误报，跳过。
            let _ = child.kill();
            let _ = child.wait();
            return;
        }

        match try_lock_file(&lock_path) {
            Err(LockError::Busy) => {}
            Err(LockError::Unavailable(reason)) => panic!("期望 Busy，实际 Unavailable: {reason}"),
            Ok(_) => panic!("跨进程必须互斥：另一个进程持锁时不应拿到锁"),
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
