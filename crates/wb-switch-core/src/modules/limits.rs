//! 模型限额台账：从**本机日志**还原「哪个账号的哪个模型被限流、官方给出的恢复时刻」。
//!
//! 不新增任何网络请求，也不解析客户端 UI 文案。窗口固定为最近 2 个日期目录；重置时刻
//! 直接采用日志原文给出的官方值，不自建限流窗口模型。
//!
//! 两档位各扫一遍（`~/.workbuddy/logs`、`~/.workbuddy-ai/logs`），一次返回全部账号的
//! 当前受限状态——扫描本身就是全局的，按账号调用会把同一份日志扫 N 遍。
//!
//! 数据流：日期目录枚举 → 字节级粗筛 → 命中才逐行解码 → 事件与模型解析 → 两步去重 →
//! 账号归因 → 按 (账号, 模型) 聚合 → 过滤掉已过官方重置时刻的条目。

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::config::now_ms;
use crate::modules::session::{open_db, table_exists, workbuddy_db_path};
use crate::modules::variant::WbVariant;

/// 日志根目录名（档位数据根下）。
const LOG_DIR_NAME: &str = "logs";

/// 扫描窗口：最近 2 个日期目录（固定，不做可配置）。
///
/// 本机实测 2 天窗口下逐行解码 0.29s、字节级粗筛 0.05s（50 文件 / 67MB），
/// 因此**不建**增量索引或本地缓存层。
const WINDOW_DAYS: usize = 2;

/// 同一事件的重置时刻相同时，发生时刻相差 ≤ 该值即视为同一次事件。
///
/// 一次 429 会在业务日志写 5–6 行、SDK 日志再写一份；本机实测 14 行只是 2 次事件，
/// 不做去重的话卡片会把 2 次事件显示成 14 条。
const MERGE_WINDOW_MS: i64 = 15_000;

/// 限额文案标记（粗筛与逐行解析共用）：中文（国内版）与英文（国际版）。
const QUOTA_MARKERS: [&str; 2] = ["超出频率限制", "usage exceeds frequency limit"];

/// 官方重置时刻的句式标记：`将在 <时间> UTC+8 重置` / `will reset at <时间> UTC+8`。
const RESET_MARKERS: [&str; 2] = ["将在 ", "will reset at "];

/// 更可靠的分类行标记：`[ACP Agent] refusal classified: …, category=quota`。
const CLASSIFIER_MARKER: &str = "refusal classified";

/// 分类行的配额判据（比 `httpStatus=429` 更贴近「限额」语义）。
const CLASSIFIER_CATEGORY: &str = "category=quota";

/// 会话当前模型标记：`[ModelConfig] sessionId=…, resolved model=…`。
const RESOLVED_MODEL_MARKER: &str = "resolved model=";

/// 业务日志行首时间格式（本地时间）：`9/17/2026, 12:20:31 AM.232`。
const BUSINESS_TS_FORMAT: &str = "%m/%d/%Y, %I:%M:%S %p%.3f";

// ---------------------------------------------------------------------------
// 扫描
// ---------------------------------------------------------------------------

/// 去重前的原始命中行。
struct Hit {
    session_id: Option<String>,
    model: Option<String>,
    reset_at: i64,
    occurred_at: i64,
}

/// 去重后的一次限额事件。
struct Event {
    session_id: Option<String>,
    model: Option<String>,
    reset_at: i64,
    first_seen_at: i64,
    hit_count: u32,
}

impl Event {
    /// 合并同一次事件的重复书写：模型保留任一有归因的值，首次出现时刻取最早，命中行数累加。
    fn merge(&mut self, other: Event) {
        if self.model.is_none() {
            self.model = other.model;
        }
        if self.session_id.is_none() {
            self.session_id = other.session_id;
        }
        self.first_seen_at = self.first_seen_at.min(other.first_seen_at);
        self.hit_count += other.hit_count;
    }
}

/// 账号归因后的条目：`(账号, 模型)` 聚合的输入。
struct Resolved {
    account_id: String,
    model: Option<String>,
    reset_at: i64,
    first_seen_at: i64,
    hit_count: u32,
}

/// 字节级粗筛：命中限额文案的文件才解码（整份文件逐行解码是本模块的主要开销）。
///
/// 用 Boyer–Moore–Horspool 的坏字符跳表：一次比较失败即可跳过多个字节，
/// 而不是退回到「每个偏移都比一次」。2 天窗口（47MB / 30 文件）实测把粗筛
/// 从 ~0.9s 降到 ~0.1s（debug 构建），是「扫描 < 1s」的主要保障。
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&last) = needle.last() else {
        return false;
    };
    if haystack.len() < needle.len() {
        return false;
    }
    // 坏字符跳表：needle 中每个字节最后一次出现处到末尾的距离。
    let mut skip = [needle.len(); 256];
    for (index, &byte) in needle[..needle.len() - 1].iter().enumerate() {
        skip[byte as usize] = needle.len() - 1 - index;
    }
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        if haystack[offset + needle.len() - 1] == last
            && &haystack[offset..offset + needle.len()] == needle
        {
            return true;
        }
        offset += skip[haystack[offset + needle.len() - 1] as usize];
    }
    false
}

/// 档位日志根下最近的日期目录（`logs/YYYY-MM-DD/`），新到旧。
///
/// 只认日期形态的目录名：`logs/` 下还有 `sdk`、`migration`、`Crash-Log` 等非日期目录。
/// 目录缺失（如国际版某天没写日志）直接跳过，不是错误。
fn recent_log_dirs(logs_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(logs_root) else {
        return Vec::new();
    };
    let mut dated: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            NaiveDate::parse_from_str(&name, "%Y-%m-%d")
                .ok()
                .map(|_| (name, path))
        })
        .collect();
    // 目录名是 ISO 日期，字典序即时间序。
    dated.sort_by(|left, right| right.0.cmp(&left.0));
    dated.truncate(WINDOW_DAYS);
    dated.into_iter().map(|(_, path)| path).collect()
}

/// 递归收集候选日志文件（日期目录下可能还有 `sdk/conversations/` 一层）。
fn log_files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            log_files(&path, output);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            output.push(path);
        }
    }
}

/// 逐行解析限额事件（文件已由粗筛确认命中）。
///
/// 顺序扫描是关键：命中限额行时，`session_models` 里恰好是「该会话在被限**之前**最近一次
/// 选用的模型」，因此不会归因成被限之后才切换到的模型。
fn scan_text(text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut session_models: HashMap<String, String> = HashMap::new();
    let mut request_models: HashMap<String, String> = HashMap::new();
    let mut classifier_session: Option<String> = None;
    for line in text.lines() {
        if let Some(model) = model_after(line, RESOLVED_MODEL_MARKER) {
            if let Some(session) = field_after(line, "sessionId=") {
                session_models.insert(session.to_string(), model.to_string());
            }
        }
        if let Some(request) = field_after(line, "requestId=") {
            if let Some(model) = request_model(line) {
                request_models.insert(request.to_string(), model.to_string());
            }
        }
        if line.contains(CLASSIFIER_MARKER) && line.contains(CLASSIFIER_CATEGORY) {
            if let Some(session) = field_after(line, "sessionId=") {
                classifier_session = Some(session.to_string());
            }
        }
        if !QUOTA_MARKERS.iter().any(|marker| line.contains(marker)) {
            continue;
        }
        let Some(reset_at) = parse_reset_at(line) else {
            continue;
        };
        // 没有发生时刻就无法参与去重：宁可丢这一行，也不猜一个时间。
        let Some(occurred_at) = line_timestamp(line) else {
            continue;
        };
        let (request_id, session_id) = match request_pair(line) {
            Some((request, session)) => (Some(request), Some(session)),
            None => (None, None),
        };
        let session_id = session_id
            .or_else(|| field_after(line, "sessionId=").map(str::to_string))
            .or_else(|| classifier_session.clone());
        // 模型归因三级回退：requestId → 该会话最近一次 model= → 未知（不猜）。
        let model = request_id
            .as_deref()
            .and_then(|request| request_models.get(request))
            .or_else(|| {
                session_id
                    .as_deref()
                    .and_then(|session| session_models.get(session))
            })
            .cloned();
        hits.push(Hit {
            session_id,
            model,
            reset_at,
            occurred_at,
        });
    }
    hits
}

fn scan_file(path: &Path, hits: &mut Vec<Hit>) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    if !QUOTA_MARKERS
        .iter()
        .any(|marker| contains(&bytes, marker.as_bytes()))
    {
        return;
    }
    hits.extend(scan_text(&String::from_utf8_lossy(&bytes)));
}

/// 枚举 → 粗筛 → 解析 → 去重，返回某个档位的限额事件。
fn collect_events(logs_root: &Path) -> Vec<Event> {
    let mut files = Vec::new();
    for directory in recent_log_dirs(logs_root) {
        log_files(&directory, &mut files);
    }
    let mut hits = Vec::new();
    for file in files {
        scan_file(&file, &mut hits);
    }
    dedupe(hits)
}

/// 两步去重：① `(会话 id, 重置时刻)` 相同即同一次事件；② 重置时刻相同且发生时刻相差
/// ≤ `MERGE_WINDOW_MS` 的记录再合并一次（业务日志与 SDK 日志各写一份，且两边的会话 id
/// 未必都取得到）。
fn dedupe(hits: Vec<Hit>) -> Vec<Event> {
    let mut groups: BTreeMap<(Option<String>, i64), Event> = BTreeMap::new();
    for hit in hits {
        let key = (hit.session_id.clone(), hit.reset_at);
        let event = Event {
            session_id: hit.session_id,
            model: hit.model,
            reset_at: hit.reset_at,
            first_seen_at: hit.occurred_at,
            hit_count: 1,
        };
        match groups.entry(key) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(event),
            Entry::Vacant(entry) => {
                entry.insert(event);
            }
        }
    }

    let mut sorted: Vec<Event> = groups.into_values().collect();
    sorted.sort_by_key(|event| (event.reset_at, event.first_seen_at));
    let mut merged: Vec<Event> = Vec::new();
    for event in sorted {
        let same_event = merged.last().is_some_and(|last| {
            last.reset_at == event.reset_at
                && event.first_seen_at - last.first_seen_at <= MERGE_WINDOW_MS
        });
        if same_event {
            if let Some(last) = merged.last_mut() {
                last.merge(event);
            }
        } else {
            merged.push(event);
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// 行解析
// ---------------------------------------------------------------------------

/// 取行首时间戳（毫秒）。业务日志是本地时间，SDK 日志是 UTC（`2026-09-16T16:20:31.523Z`），
/// 两者都折算到同一绝对时间轴后再比较。
fn line_timestamp(line: &str) -> Option<i64> {
    let trimmed = line.trim_start();
    let head = trimmed.split_whitespace().next().unwrap_or_default();
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(head) {
        return Some(parsed.timestamp_millis());
    }
    let rest = trimmed.strip_prefix('[')?;
    let end = rest.find(']')?;
    let naive = NaiveDateTime::parse_from_str(rest[..end].trim(), BUSINESS_TS_FORMAT).ok()?;
    // DST 回拨的那一小时是歧义的：取较早的一次，而不是丢掉这一行。
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|date| date.timestamp_millis())
}

/// 官方给出的重置时刻（毫秒）。原文形如 `将在 2026-09-17 17:59:27 UTC+8 重置` /
/// `will reset at 2026-09-14 10:59:21 UTC+8,`。
///
/// 直接采用原文值（含原文声明的时区偏移），不自建窗口模型。
fn parse_reset_at(line: &str) -> Option<i64> {
    for marker in RESET_MARKERS {
        let Some(index) = line.find(marker) else {
            continue;
        };
        let rest = &line[index + marker.len()..];
        let Some(naive) = parse_datetime_prefix(rest) else {
            continue;
        };
        let Some(offset) = parse_utc_offset(rest) else {
            continue;
        };
        return offset
            .from_local_datetime(&naive)
            .single()
            .map(|date| date.timestamp_millis());
    }
    None
}

/// 取前缀里的 `YYYY-MM-DD HH:MM:SS`。
fn parse_datetime_prefix(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text.get(..19)?, "%Y-%m-%d %H:%M:%S").ok()
}

/// 解析文案里的 `UTC±H[:MM]` 偏移。
fn parse_utc_offset(text: &str) -> Option<FixedOffset> {
    let index = text.find("UTC")?;
    let rest = &text[index + 3..];
    let sign = match rest.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = rest[1..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours.parse::<i32>().ok()?, minutes.parse::<i32>().ok()?),
        None => match digits.len() {
            1 | 2 => (digits.parse::<i32>().ok()?, 0),
            4 => (
                digits[..2].parse::<i32>().ok()?,
                digits[2..].parse::<i32>().ok()?,
            ),
            _ => return None,
        },
    };
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

/// 取行尾 `(requestId/sessionId)`：32 位 hex 请求 id + 带连字符的 36 位会话 uuid。
///
/// 括号里那串 hex 是 **requestId，不是账号**；账号归因只能走会话 id。
fn request_pair(line: &str) -> Option<(String, String)> {
    let mut found = None;
    let mut search = 0;
    while let Some(offset) = line[search..].find('(') {
        let start = search + offset + 1;
        let Some(end) = line[start..].find(')') else {
            break;
        };
        if let Some((request, session)) = line[start..start + end].split_once('/') {
            if is_hex_id(request, 32) && is_hex_id(session, 36) {
                found = Some((request.to_string(), session.to_string()));
            }
        }
        search = start + end;
    }
    found
}

fn is_hex_id(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

/// 取 `<key>` 字段值（到 `,` / 空格 / 引号 / 右括号为止）。
///
/// 要求 key 前是字段边界，避免 `parentSessionId=` 之类的相似字段误命中。
fn field_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let mut search = 0;
    while let Some(offset) = line[search..].find(key) {
        let index = search + offset;
        let boundary = index == 0 || !is_field_char(line.as_bytes()[index - 1]);
        if boundary {
            let value = line[index + key.len()..]
                .split([',', ' ', '"', '}', ')'])
                .next()
                .unwrap_or_default()
                .trim();
            if !value.is_empty() {
                return Some(value);
            }
        }
        search = index + key.len();
    }
    None
}

fn is_field_char(byte: u8) -> bool {
    // 点号也算字段字符：`requestOptions.model=` 不是 `model=` 字段。
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.'
}

/// 取 `<marker>` 之后的模型名。
fn model_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let start = line.find(marker)? + marker.len();
    let value = line[start..].split([',', ' ', ';']).next()?.trim();
    (!value.is_empty()).then_some(value)
}

/// 取 `requestId → model` 映射里这一行给出的模型名。
///
/// 只认字段边界上的 `model=`：`[ModelProvider] Sending request: agent=cli,
/// model=<model>, requestId=<id>` 命中，而 `requestOptions.model=` 不命中。
fn request_model(line: &str) -> Option<&str> {
    let mut search = 0;
    while let Some(offset) = line[search..].find("model=") {
        let index = search + offset;
        if index == 0 || !is_field_char(line.as_bytes()[index - 1]) {
            return model_after(&line[index..], "model=");
        }
        search = index + "model=".len();
    }
    None
}

// ---------------------------------------------------------------------------
// 汇总
// ---------------------------------------------------------------------------

/// sessionId → 账号 id。
///
/// 归因失败（`sessions` 表缺失、会话不在库、uid 未收录）时返回空映射，调用方据此丢弃
/// 该事件——宁可少显示，不可显示错账号。
fn account_by_session(
    variant: WbVariant,
    session_ids: &BTreeSet<String>,
) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    if session_ids.is_empty() {
        return mapping;
    }
    let uid_to_account: HashMap<String, String> = account::load_accounts()
        .iter()
        .filter(|acc| account::variant_of(acc) == variant)
        .filter_map(|acc| Some((account::get_str(acc, "uid")?, account::get_str(acc, "id")?)))
        .collect();
    if uid_to_account.is_empty() {
        return mapping;
    }
    let db = workbuddy_db_path(variant);
    if !db.is_file() {
        return mapping;
    }
    let Some(conn) = open_db(&db, true) else {
        return mapping;
    };
    if !table_exists(&conn, "sessions") {
        return mapping;
    }
    // 批量查询：逐条查库在事件多时会成为主要开销。
    let placeholders = vec!["?"; session_ids.len()].join(",");
    let sql = format!("SELECT id, user_id FROM sessions WHERE id IN ({placeholders})");
    let Ok(mut statement) = conn.prepare(&sql) else {
        return mapping;
    };
    let Ok(rows) = statement.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ))
    }) else {
        return mapping;
    };
    for (session_id, user_id) in rows.flatten() {
        let (Some(session_id), Some(user_id)) = (session_id, user_id) else {
            continue;
        };
        if let Some(account_id) = uid_to_account.get(&user_id) {
            mapping.insert(session_id, account_id.clone());
        }
    }
    mapping
}

/// 账号归因：归因不到账号的事件直接丢弃（不错归给任何账号）。
fn resolve(variant: WbVariant, events: &[Event]) -> Vec<Resolved> {
    let session_ids: BTreeSet<String> = events
        .iter()
        .filter_map(|event| event.session_id.clone())
        .collect();
    let mapping = account_by_session(variant, &session_ids);
    events
        .iter()
        .filter_map(|event| {
            let account_id = event
                .session_id
                .as_deref()
                .and_then(|session| mapping.get(session))?;
            Some(Resolved {
                account_id: account_id.clone(),
                model: event.model.clone(),
                reset_at: event.reset_at,
                first_seen_at: event.first_seen_at,
                hit_count: event.hit_count,
            })
        })
        .collect()
}

/// 聚合与过滤：per (账号, 模型) 取 `resetAt` 最大的一条（同模型多次限流以最新一次为准），
/// 只保留官方重置时刻尚未到达的条目。
///
/// 无受限模型的账号**不出现在结果里**，前端据此决定是否渲染图标。
fn build_payload(resolved: Vec<Resolved>, scanned_at: i64) -> Value {
    let mut latest: BTreeMap<(String, Option<String>), Resolved> = BTreeMap::new();
    for item in resolved {
        if item.reset_at <= scanned_at {
            continue;
        }
        match latest.entry((item.account_id.clone(), item.model.clone())) {
            Entry::Occupied(mut entry) => {
                if item.reset_at > entry.get().reset_at {
                    entry.insert(item);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(item);
            }
        }
    }

    let mut by_account: BTreeMap<String, Vec<Resolved>> = BTreeMap::new();
    for item in latest.into_values() {
        by_account
            .entry(item.account_id.clone())
            .or_default()
            .push(item);
    }
    let accounts: Vec<Value> = by_account
        .into_iter()
        .map(|(account_id, mut items)| {
            // 按恢复时间升序：最早解锁的排在最前（最有行动价值）。
            items.sort_by_key(|item| item.reset_at);
            let limited: Vec<Value> = items
                .into_iter()
                .map(|item| {
                    json!({
                        "model": item.model,
                        "resetAt": item.reset_at,
                        "firstSeenAt": item.first_seen_at,
                        "hitCount": item.hit_count,
                    })
                })
                .collect();
            json!({ "accountId": account_id, "limited": limited })
        })
        .collect();
    json!({
        "scannedAt": scanned_at,
        "windowDays": WINDOW_DAYS,
        "accounts": accounts,
    })
}

/// 全部账号当前的模型限额状态（两档位各扫一遍）。
///
/// 不接收档位参数：扫描本身就是全局的，按档位调用会把同一份日志扫 N 遍。
pub fn get_rate_limits() -> Value {
    let scanned_at = now_ms();
    let mut resolved = Vec::new();
    for variant in WbVariant::ALL {
        let events = collect_events(&variant.data_root().join(LOG_DIR_NAME));
        resolved.extend(resolve(variant, &events));
    }
    build_payload(resolved, scanned_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机实测的重置时刻：`2026-09-17 17:59:27 UTC+8`。
    const CN_RESET_AT: i64 = 1_789_639_167_000;
    /// 国际版文案样例的重置时刻：`2026-09-14 10:59:21 UTC+8`。
    const EN_RESET_AT: i64 = 1_789_354_761_000;
    /// 本机实测的 SDK 行时间戳：`2026-09-16T16:20:31.523Z`。
    const SDK_OCCURRED_AT: i64 = 1_789_575_631_523;

    const CN_SESSION: &str = "387a486d-b5c9-473d-b823-121db59f0084";
    const CN_REQUEST: &str = "34fccb8a1ed74da39dea5481eba85b46";
    const EN_SESSION: &str = "9b9130df-3665-4fdc-9ae7-65c23c8f8edd";
    const EN_REQUEST: &str = "4d83788eb9de41d08424eb2b64c08bd3";

    fn business_line(timestamp: &str, body: &str) -> String {
        format!("[{timestamp}] [Error] [pid=1] {body}")
    }

    /// 中文限额文案（`session` / `request` / `reset` 可替换，便于构造多条事件）。
    fn cn_quota_with(session: &str, request: &str, reset: &str) -> String {
        format!(
            "429 您的使用量已超出频率限制，将在 {reset} UTC+8 重置，您也可以切换其他模型继续使用。 ({request}/{session})"
        )
    }

    fn cn_quota() -> String {
        cn_quota_with(CN_SESSION, CN_REQUEST, "2026-09-17 17:59:27")
    }

    fn en_quota() -> String {
        format!(
            "429 usage exceeds frequency limit, please try later, will reset at 2026-09-14 10:59:21 UTC+8, switch to another model. ({EN_REQUEST}/{EN_SESSION})"
        )
    }

    fn config_line(session: &str, model: &str) -> String {
        format!("[ModelConfig] sessionId={session}, resolved model={model} for agent=cli, requestOptions.model={model}")
    }

    fn provider_line(request: &str, model: &str) -> String {
        format!(
            "[ModelProvider] Sending request: agent=cli, model={model}, requestId={request}, stream=true"
        )
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wb-switch-limits-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn byte_filter_matches_multibyte_and_ascii_markers() {
        assert!(contains(cn_quota().as_bytes(), QUOTA_MARKERS[0].as_bytes()));
        assert!(contains(
            b"x usage exceeds frequency limit y",
            QUOTA_MARKERS[1].as_bytes()
        ));
        assert!(!contains(b"nothing to see", QUOTA_MARKERS[1].as_bytes()));
        assert!(!contains(b"", QUOTA_MARKERS[0].as_bytes()));
        assert!(!contains(b"", b""));
        // Horspool 跳表的边界：needle 比 haystack 长、needle 内含重复字节、needle 在末尾命中。
        assert!(!contains(b"aaa", b"aaaa"));
        assert!(contains(b"aaaa", b"aaaa"));
        assert!(contains(b"abcabcabd", b"abcabd"));
        assert!(contains(b"xxababacyy", b"ababac"));
        assert!(!contains(b"xxabababyy", b"ababac"));
    }

    #[test]
    fn parses_chinese_and_english_reset_times() {
        assert_eq!(parse_reset_at(&cn_quota()), Some(CN_RESET_AT));
        assert_eq!(parse_reset_at(&en_quota()), Some(EN_RESET_AT));
        // 缺时区偏移的文案不猜时间。
        assert_eq!(
            parse_reset_at("429 您的使用量已超出频率限制，将在 2026-09-17 17:59:27 重置"),
            None
        );
    }

    #[test]
    fn parses_utc_offsets_the_official_text_may_use() {
        let at = |offset: &str| {
            parse_reset_at(&format!(
                "将在 2026-09-17 17:59:27 UTC{offset} 重置，您也可以切换其他模型继续使用。"
            ))
        };
        assert_eq!(at("+8"), Some(CN_RESET_AT));
        assert_eq!(at("+08"), Some(CN_RESET_AT));
        assert_eq!(at("+08:00"), Some(CN_RESET_AT));
        assert_eq!(at("+0800"), Some(CN_RESET_AT));
        assert_eq!(at("+7"), Some(CN_RESET_AT + 3_600_000));
        assert_eq!(at(""), None);
    }

    #[test]
    fn business_log_timestamp_is_local_while_sdk_log_timestamp_is_utc() {
        let local = Local
            .with_ymd_and_hms(2026, 9, 17, 0, 20, 31)
            .earliest()
            .expect("测试时间必须存在");
        assert_eq!(
            line_timestamp(&business_line("9/17/2026, 12:20:31 AM.232", "[Info] x")),
            Some(local.timestamp_millis() + 232)
        );
        assert_eq!(
            line_timestamp("2026-09-16T16:20:31.523Z runtime.applyStopReason {}"),
            Some(SDK_OCCURRED_AT)
        );
        assert_eq!(line_timestamp("no timestamp here"), None);
    }

    #[test]
    fn parses_the_trailing_request_id_pair() {
        assert_eq!(
            request_pair(&cn_quota()),
            Some((CN_REQUEST.to_string(), CN_SESSION.to_string()))
        );
        assert_eq!(
            request_pair(&format!(
                "x ({CN_REQUEST}/{CN_SESSION}), lastPendingTool=(none)"
            )),
            Some((CN_REQUEST.to_string(), CN_SESSION.to_string())),
            "取行尾括号，忽略 `(none)` 这种非时间戳括号"
        );
        assert_eq!(request_pair("no pair here"), None);
        assert_eq!(
            request_pair(&format!("({CN_REQUEST}/{CN_REQUEST})")),
            None,
            "会话 id 不是 36 位 uuid 时不认"
        );
    }

    #[test]
    fn model_attribution_falls_back_request_id_then_session_then_unknown() {
        // ① 限额行给出 requestId，同文件有 `model=` + `requestId=` 的行。
        let with_request = [
            business_line(
                "9/17/2026, 12:20:03 AM.349",
                &config_line(CN_SESSION, "hy3"),
            ),
            business_line(
                "9/17/2026, 12:20:30 AM.838",
                &provider_line(CN_REQUEST, "deepseek-v4.1-flash"),
            ),
            business_line("9/17/2026, 12:20:31 AM.232", &cn_quota()),
        ]
        .join("\n");
        let events = dedupe(scan_text(&with_request));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("deepseek-v4.1-flash"));

        // ② 没有 requestId → model 行，回退到该会话最近一次 `resolved model=`。
        let with_session = [
            business_line(
                "9/17/2026, 12:20:03 AM.349",
                &config_line(CN_SESSION, "hy3"),
            ),
            business_line("9/17/2026, 12:20:31 AM.232", &cn_quota()),
        ]
        .join("\n");
        let events = dedupe(scan_text(&with_session));
        assert_eq!(events[0].model.as_deref(), Some("hy3"));

        // ③ 都取不到 → None（前端显示「未知模型」，绝不猜）。
        let without_model = business_line("9/17/2026, 12:20:31 AM.232", &cn_quota());
        let events = dedupe(scan_text(&without_model));
        assert_eq!(events[0].model, None);
    }

    #[test]
    fn attributes_the_limited_model_not_the_one_switched_to_afterwards() {
        let text = [
            business_line(
                "9/17/2026, 12:20:03 AM.349",
                &config_line(CN_SESSION, "hy3"),
            ),
            business_line(
                "9/17/2026, 12:20:30 AM.838",
                &provider_line(CN_REQUEST, "deepseek-v4.1-flash"),
            ),
            business_line("9/17/2026, 12:20:31 AM.232", &cn_quota()),
            // 被限之后用户切到了 hy3：不得归因成 hy3。
            business_line(
                "9/17/2026, 12:21:54 AM.388",
                &config_line(CN_SESSION, "hy3"),
            ),
        ]
        .join("\n");
        let events = dedupe(scan_text(&text));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("deepseek-v4.1-flash"));
    }

    #[test]
    fn request_options_model_is_not_mistaken_for_the_request_model() {
        let text = [
            business_line(
                "9/17/2026, 12:20:03 AM.349",
                &config_line("other", "kimi-k3-1"),
            ),
            business_line(
                "9/17/2026, 12:20:30 AM.838",
                &format!("[ModelProvider] requestId={CN_REQUEST}, requestOptions.model=glm-5.2"),
            ),
            business_line("9/17/2026, 12:20:31 AM.232", &cn_quota()),
        ]
        .join("\n");
        let events = dedupe(scan_text(&text));
        assert_eq!(
            events[0].model, None,
            "带点号的 `requestOptions.model=` 不是请求模型字段"
        );
    }

    #[test]
    fn classifier_line_supplies_the_session_when_the_pair_is_missing() {
        let text = [
            business_line(
                "9/17/2026, 12:20:31 AM.467",
                &format!(
                    "[ACP Agent] refusal classified: sessionId={CN_SESSION}, rpcCode=-32003, httpStatus=429, bizCode=6004, category=quota"
                ),
            ),
            business_line(
                "9/17/2026, 12:20:31 AM.468",
                "429 您的使用量已超出频率限制，将在 2026-09-17 17:59:27 UTC+8 重置，您也可以切换其他模型继续使用。",
            ),
        ]
        .join("\n");
        let events = dedupe(scan_text(&text));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id.as_deref(), Some(CN_SESSION));
    }

    #[test]
    fn recognizes_english_quota_text() {
        let text = [
            business_line(
                "9/14/2026, 10:59:00 AM.000",
                &config_line(EN_SESSION, "kimi-k3-1"),
            ),
            business_line("9/14/2026, 10:59:21 AM.523", &en_quota()),
        ]
        .join("\n");
        let events = dedupe(scan_text(&text));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reset_at, EN_RESET_AT);
        assert_eq!(events[0].model.as_deref(), Some("kimi-k3-1"));
        assert_eq!(events[0].session_id.as_deref(), Some(EN_SESSION));
    }

    /// AC4：本机最近 3 天 14 条限额行只是 2 次事件（每次事件业务日志 6 行 + SDK 冗余 1 行）。
    #[test]
    fn fourteen_duplicate_lines_collapse_into_two_events() {
        let root = temp_dir("dedupe");
        let business = root.join("2026-09-16");
        let sdk = root.join("2026-09-17").join("sdk").join("conversations");
        std::fs::create_dir_all(&business).expect("业务日志目录");
        std::fs::create_dir_all(&sdk).expect("SDK 日志目录");

        let event_lines = |session: &str,
                           request: &str,
                           reset: &str,
                           provider_at: &str,
                           quota_at: &str,
                           switched_at: &str| {
            let mut lines = vec![
                business_line(provider_at, &config_line(session, "deepseek-v4.1-flash")),
                business_line(provider_at, &provider_line(request, "deepseek-v4.1-flash")),
            ];
            for ms in ["232", "233", "233", "462", "467", "468"] {
                lines.push(business_line(
                    &format!("{quota_at}.{ms}"),
                    &cn_quota_with(session, request, reset),
                ));
            }
            lines.push(business_line(switched_at, &config_line(session, "hy3")));
            lines
        };

        // 与真机一致：两次事件的**重置时刻相同**，靠发生时刻相差 805s（≫15s 合并窗口）
        // 保持为两次事件——若这里用不同的重置时刻区分，就测不到「同 reset 不得误合并」。
        let first = event_lines(
            CN_SESSION,
            CN_REQUEST,
            "2026-09-17 17:59:27",
            "9/17/2026, 12:20:03 AM.349",
            "9/17/2026, 12:20:31 AM",
            "9/17/2026, 12:21:54 AM.388",
        );
        std::fs::write(business.join("event-one.log"), first.join("\n")).expect("写入业务日志");
        let second = event_lines(
            EN_SESSION,
            EN_REQUEST,
            "2026-09-17 17:59:27",
            "9/17/2026, 12:33:56 AM.520",
            "9/17/2026, 12:33:56 AM",
            "9/17/2026, 1:25:24 AM.523",
        );
        std::fs::write(business.join("event-two.log"), second.join("\n")).expect("写入业务日志");

        // SDK 侧对同一事件再写一份（UTC 时间戳、无模型行），会话 id 仍取得到。
        // 两次书写各随其业务日志的时刻（真机相差约 0.3s），否则两条 SDK 冗余会把
        // 相隔 13 分钟的事件拉进同一个 15s 窗口。
        for (session, quota, sdk_at) in [
            (CN_SESSION, &first[2], "2026-09-16T16:20:31.523Z"),
            (EN_SESSION, &second[2], "2026-09-16T16:33:56.520Z"),
        ] {
            std::fs::write(
                sdk.join(format!("{session}.log")),
                format!(
                    "{sdk_at} runtime.applyStopReason {{\"errorMessageMetaPreview\":\"{quota}\"}}"
                ),
            )
            .expect("写入 SDK 日志");
        }

        let events = collect_events(&root);
        assert_eq!(events.len(), 2, "14 条限额行必须聚合为 2 次事件");
        assert_eq!(events[0].hit_count, 7);
        assert_eq!(events[1].hit_count, 7);
        assert_eq!(events[0].model.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(events[1].model.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(events[0].reset_at, CN_RESET_AT);
        assert_eq!(
            events[1].reset_at, CN_RESET_AT,
            "两次事件重置时刻相同，仍必须是两次事件"
        );
        assert_eq!(events[0].session_id.as_deref(), Some(CN_SESSION));
        assert_eq!(events[1].session_id.as_deref(), Some(EN_SESSION));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dedupe_keeps_sessions_apart_and_merges_only_within_fifteen_seconds() {
        let hit = |session: &str, occurred_at: i64, model: Option<&str>| Hit {
            session_id: Some(session.to_string()),
            model: model.map(str::to_string),
            reset_at: CN_RESET_AT,
            occurred_at,
        };
        let events = dedupe(vec![
            // ① 步：同一会话的重复书写先合并。
            hit("session-a", 1_000_000, None),
            hit("session-a", 1_014_000, None),
            // ② 步：不同会话、重置时刻相同且相差 ≤15s，属同一次事件的另一次书写。
            hit("session-b", 1_003_000, Some("hy3")),
            // 相差 20s（超出合并窗口）的另一次限流不得被吞。
            hit("session-c", 1_020_000, None),
        ]);
        assert_eq!(events.len(), 2, "相差 20s 的第二次限流不得被合并");
        assert_eq!(events[0].hit_count, 3);
        assert_eq!(events[0].session_id.as_deref(), Some("session-a"));
        assert_eq!(
            events[0].model.as_deref(),
            Some("hy3"),
            "合并时保留有归因的那条"
        );
        assert_eq!(events[1].hit_count, 1);
        assert_eq!(events[1].session_id.as_deref(), Some("session-c"));
    }

    #[test]
    fn window_keeps_the_two_newest_date_directories_and_tolerates_missing_ones() {
        let root = temp_dir("window");
        for name in ["2026-09-11", "2026-09-16", "2026-09-17"] {
            std::fs::create_dir_all(root.join(name)).expect("日期目录");
        }
        // `logs/` 下的非日期目录必须被忽略。
        std::fs::create_dir_all(root.join("migration")).expect("非日期目录");
        let names: Vec<String> = recent_log_dirs(&root)
            .iter()
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().to_string()))
            .collect();
        assert_eq!(names, ["2026-09-17", "2026-09-16"]);
        assert!(
            recent_log_dirs(&root.join("missing")).is_empty(),
            "档位日志根缺失时返回空集，不是错误"
        );
        assert!(collect_events(&root.join("missing")).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn payload_drops_expired_entries_and_sorts_by_reset_time() {
        let now = 1_000_000;
        let resolved = vec![
            Resolved {
                account_id: "a".to_string(),
                model: Some("late".to_string()),
                reset_at: now + 7_200_000,
                first_seen_at: 0,
                hit_count: 1,
            },
            Resolved {
                account_id: "a".to_string(),
                model: Some("soon".to_string()),
                reset_at: now + 600_000,
                first_seen_at: 0,
                hit_count: 3,
            },
            Resolved {
                account_id: "a".to_string(),
                model: Some("expired".to_string()),
                reset_at: now - 1,
                first_seen_at: 0,
                hit_count: 1,
            },
            Resolved {
                account_id: "b".to_string(),
                model: None,
                reset_at: now + 60_000,
                first_seen_at: 0,
                hit_count: 2,
            },
        ];
        let payload = build_payload(resolved, now);
        assert_eq!(payload["windowDays"], 2);
        assert_eq!(payload["scannedAt"], now);
        let accounts = payload["accounts"].as_array().expect("accounts 数组");
        assert_eq!(accounts.len(), 2, "无受限模型的账号不出现在结果里");
        assert_eq!(accounts[0]["accountId"], "a");
        let limited = accounts[0]["limited"].as_array().expect("limited 数组");
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0]["model"], "soon", "按恢复时间升序");
        assert_eq!(limited[1]["model"], "late");
        assert_eq!(accounts[1]["limited"][0]["model"], Value::Null);
    }

    #[test]
    fn payload_keeps_the_latest_reset_time_per_account_and_model() {
        let now = 1_000_000;
        let resolved = vec![
            Resolved {
                account_id: "a".to_string(),
                model: Some("hy3".to_string()),
                reset_at: now + 600_000,
                first_seen_at: 10,
                hit_count: 1,
            },
            Resolved {
                account_id: "a".to_string(),
                model: Some("hy3".to_string()),
                reset_at: now + 3_600_000,
                first_seen_at: 20,
                hit_count: 2,
            },
        ];
        let payload = build_payload(resolved, now);
        let limited = payload["accounts"][0]["limited"]
            .as_array()
            .expect("limited 数组");
        assert_eq!(limited.len(), 1, "同 (账号, 模型) 只保留一条");
        assert_eq!(limited[0]["resetAt"], now + 3_600_000);
        assert_eq!(limited[0]["hitCount"], 2);
    }
}
