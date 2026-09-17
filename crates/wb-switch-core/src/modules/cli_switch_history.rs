//! CLI 活跃账号切换历史（限额归因用）。
//!
//! CodeBuddy CLI 的 key 由 `helper.cjs` 注入后**按进程快照**：`switch_active_account`
//! 只对新进程生效（其提示语即声明「当前运行会话不会切换」）。因此 hook 限额事件不能看
//! 「事件时刻的 `state.json`」，而要看「该会话当前进程的启动时刻」当时生效的账号——
//! 那一刻的账号只能由本文件（每次切换成功时记下的历史）回答。
//!
//! 文件形态是 append-only JSONL：`{"at":<ms>,"accountId":"<id>"}`。写入是尽力而为
//! （失败只告警，不影响切换本身）；读取容忍坏行/半行，失败一律当作「无历史」，
//! 归因侧据此保守丢弃。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::modules::config::{atomic_write, store_dir};

/// 历史文件名（`store_dir()/cli_switch_history.jsonl`）。
const HISTORY_FILE_NAME: &str = "cli_switch_history.jsonl";

/// 保留窗口：90 天。比任何活着的 CLI 进程都长，超龄记录不可能再被归因用到。
const HISTORY_MAX_AGE_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// 条数上限（双保险：频繁切换也不让文件无限增长）。
const HISTORY_MAX_ENTRIES: usize = 2000;

/// 一条切换记录（`at` = 切换生效时刻，与 state.json 的 `updatedAt` 同一时刻）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchRecord {
    at: i64,
    account_id: String,
}

/// CLI 切换历史文件路径。
pub(crate) fn history_path() -> PathBuf {
    store_dir().join(HISTORY_FILE_NAME)
}

/// 读取全部有效记录（文件缺失、坏行、写到一半的行一律忽略；保持写入顺序）。
fn read_records(path: &Path) -> Vec<SwitchRecord> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<SwitchRecord>(line.trim()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 追加一条切换记录；与最后一条同账号则跳过，并按窗口 + 条数上限裁剪。
///
/// 幂等只在「末尾同账号」这一层：A → B → A 的往返切换各自都要留下记录。
/// 返回的错误由调用方决定是否上报（切换本身已经成功，不因历史失败而回滚）。
pub(crate) fn append(path: &Path, account_id: &str, at: i64) -> std::io::Result<()> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(());
    }
    let mut records = read_records(path);
    if records
        .last()
        .is_some_and(|last| last.account_id == account_id)
    {
        return Ok(());
    }
    records.push(SwitchRecord {
        at,
        account_id: account_id.to_string(),
    });
    // 按时刻排序再裁：文件若曾乱序（时钟回拨、手工编辑），条数上限要留「最新」而非「文件末尾」。
    records.sort_by_key(|record| record.at);
    let cutoff = at.saturating_sub(HISTORY_MAX_AGE_MS);
    records.retain(|record| record.at >= cutoff);
    if records.len() > HISTORY_MAX_ENTRIES {
        records.drain(..records.len() - HISTORY_MAX_ENTRIES);
    }
    let mut content = String::new();
    for record in &records {
        content.push_str(&serde_json::to_string(record).unwrap_or_default());
        content.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(path, &content)
}

/// `at` 时刻生效的账号（`record.at <= at` 里时刻最大的那条；同刻取文件中后出现的）。
///
/// 按时刻而不是文件顺序：乱序写入时「文件最后一条」不一定是最近一次切换。
/// 无记录 / 全部晚于 `at` → None：调用方据此回落 mtime 判据或丢弃，不得猜。
pub(crate) fn account_at(path: &Path, at: i64) -> Option<String> {
    read_records(path)
        .into_iter()
        .filter(|record| record.at <= at)
        .max_by_key(|record| record.at)
        .map(|record| record.account_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "wb-switch-{name}-{}",
                uuid::Uuid::new_v4().simple()
            ))
            .join(HISTORY_FILE_NAME)
    }

    #[test]
    fn append_writes_camel_case_jsonl_and_creates_the_parent_directory() {
        let path = temp_path("cli-switch-history-write");
        assert!(!path.parent().unwrap().exists());
        append(&path, "acc-a", 1_000).expect("追加");
        assert_eq!(
            std::fs::read_to_string(&path).expect("历史文件"),
            "{\"at\":1000,\"accountId\":\"acc-a\"}\n"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn append_skips_same_account_but_keeps_round_trips() {
        let path = temp_path("cli-switch-history-idempotent");
        append(&path, "acc-a", 1_000).expect("追加");
        // 同值切换不重复记录。
        append(&path, "acc-a", 2_000).expect("追加");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        // A → B → A 的往返各自留痕（快照时刻判据不能把这两段合并）。
        append(&path, "acc-b", 3_000).expect("追加");
        append(&path, "acc-a", 4_000).expect("追加");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
        assert_eq!(account_at(&path, 4_000).as_deref(), Some("acc-a"));
        assert_eq!(account_at(&path, 3_500).as_deref(), Some("acc-b"));
        assert_eq!(account_at(&path, 2_500).as_deref(), Some("acc-a"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn account_at_handles_boundaries_and_missing_files() {
        let path = temp_path("cli-switch-history-boundaries");
        append(&path, "acc-a", 1_000).expect("追加");
        append(&path, "acc-b", 2_000).expect("追加");
        assert_eq!(
            account_at(&path, 1_000).as_deref(),
            Some("acc-a"),
            "恰等于记录时刻"
        );
        assert_eq!(account_at(&path, 1_999).as_deref(), Some("acc-a"));
        assert_eq!(account_at(&path, 999), None, "早于全部记录 → 无历史覆盖");
        assert_eq!(
            account_at(&path, 9_999).as_deref(),
            Some("acc-b"),
            "晚于全部"
        );
        let missing = path.parent().unwrap().join("missing.jsonl");
        assert_eq!(account_at(&missing, 9_999), None, "文件缺失 = 无历史");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// 文件顺序乱了也要按时刻取「最后一个 ≤ at」的账号，不能取文件末行。
    #[test]
    fn account_at_uses_the_latest_timestamp_not_file_order() {
        let path = temp_path("cli-switch-history-out-of-order");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"at\":3000,\"accountId\":\"acc-new\"}\n{\"at\":1000,\"accountId\":\"acc-old\"}\n",
        )
        .unwrap();
        assert_eq!(
            account_at(&path, 2_000).as_deref(),
            Some("acc-old"),
            "2000 时刻只能看到 1000 那条"
        );
        assert_eq!(
            account_at(&path, 3_000).as_deref(),
            Some("acc-new"),
            "不能因为 acc-old 写在文件末尾就覆盖较新的切换"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn bad_lines_are_ignored_and_dropped_on_the_next_write() {
        let path = temp_path("cli-switch-history-bad-lines");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "not-json\n{\"at\":1000,\"accountId\":\"acc-a\"}\n{\"at\":20\n\n",
        )
        .unwrap();
        assert_eq!(account_at(&path, 9_999).as_deref(), Some("acc-a"));
        // 坏行不阻塞后续追加（重写时自然被清掉）。
        append(&path, "acc-b", 2_000).expect("追加");
        assert_eq!(account_at(&path, 9_999).as_deref(), Some("acc-b"));
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn append_prunes_expired_and_excess_records() {
        let path = temp_path("cli-switch-history-prune");
        // 超龄：91 天前的记录在下次追加时被裁掉。
        append(&path, "acc-old", 0).expect("追加");
        let at = HISTORY_MAX_AGE_MS + 1;
        append(&path, "acc-new", at).expect("追加");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        assert_eq!(account_at(&path, at).as_deref(), Some("acc-new"));

        // 超量：只保留最近 HISTORY_MAX_ENTRIES 条。
        for index in 0..HISTORY_MAX_ENTRIES + 10 {
            append(&path, &format!("acc-{index}"), at + index as i64 + 1).expect("追加");
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            HISTORY_MAX_ENTRIES
        );
        assert_eq!(
            account_at(&path, at + 1_000_000).as_deref(),
            Some(format!("acc-{}", HISTORY_MAX_ENTRIES + 9).as_str())
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn empty_account_id_is_a_no_op() {
        let path = temp_path("cli-switch-history-empty-id");
        append(&path, "  ", 1_000).expect("追加");
        assert!(!path.exists(), "空账号不产生记录");
    }

    #[test]
    fn history_path_lives_in_the_store_directory() {
        assert!(history_path().ends_with(HISTORY_FILE_NAME));
    }
}
