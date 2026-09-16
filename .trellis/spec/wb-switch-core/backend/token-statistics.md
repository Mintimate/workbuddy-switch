# Local Token Statistics Contract

> Applies to: both variants (`cn`, `ai`); WorkBuddy is split into a domestic
> (`workbuddy`) and an international (`workbuddy-ai`) source, reported
> independently.

## Scenario: WorkBuddy (CN/AI), CodeBuddy CLI, and CodeBuddy IDE token dashboard

### 1. Scope / Trigger

- Trigger: the token dashboard adds a cross-layer local aggregation API.
- Scope: decode usage records from the four local sources, aggregate safe numeric
  projections, and expose the same payload through Tauri and HTTP.
- The domestic (`workbuddy`) and international (`workbuddy-ai`) WorkBuddy
  sources are separate: their roots differ, they are never mixed, and each can
  be empty independently.
- The decoder in `crates/wb-switch-core/src/modules/token_stats.rs` is the sole
  owner of event parsing, source isolation, filtering, and aggregation.

### 2. Signatures

- Core: `token_stats::get_statistics(days: Option<i64>) -> serde_json::Value`.
- Tauri: `get_token_statistics(days: Option<i64>) -> Result<serde_json::Value, String>`;
  the command reports blocking-worker join failures through the `Err` branch.
- HTTP: `GET /api/token-stats` with optional query parameter `days`.
- Accepted range values are `7`, `30`, and `90`; all other values mean the
  complete available local history (`rangeDays: null`).
- Internal JSONL collector: `source(root, name, cutoff, detail: bool)`. Only the
  CodeBuddy CLI source passes `detail = true`; the flag gates the optional
  `requests` array and changes nothing else.
- Detail constants: `REQUEST_WINDOW = 500` (rows returned per response) and
  `REQUEST_COLLECT_LIMIT = 4_000` (in-scan memory cap).

### 3. Contracts

#### Request

- `days` is an optional signed integer at the HTTP/Tauri boundary.
- Sources are fixed local roots:
  - WorkBuddy (CN) JSONL: `~/.workbuddy/projects`
  - WorkBuddy AI (international) JSONL: `~/.workbuddy-ai/projects` and
    `~/.workbuddy-ai/sessions`; each root is scanned only when it exists
    (`token_stats::jsonl_root_candidates` / `variant_source_roots`). The two
    WorkBuddy roots are not assumed to have the same layout.
  - CodeBuddy CLI JSONL: `~/.codebuddy/projects`
  - CodeBuddy IDE conversation indexes:
    `{data_local_dir}/CodeBuddyExtension/Data/**/history/{workspace}/{conversation}/index.json`
    (`data_local_dir` is `~/Library/Application Support` on macOS,
    `%LOCALAPPDATA%` on Windows, and `~/.local/share` on Linux).
- CodeBuddy IDE does not use JSONL. Usage lives on
  `requests[].usage` (`inputTokens`, `outputTokens`, `cacheTokens`,
  `cachedWriteTokens`) with `startedAt` as the record timestamp.
  Workspace `index.json` supplies optional `name` / `selectedModelId`.
  Directories named `messages`, `check-point`, `backups`, or `Public` are
  not scanned, so chat bodies are never read.

#### Response

```json
{
  "generatedAt": 0,
  "rangeDays": 7,
  "sources": [{
    "source": "workbuddy",
    "summary": {
      "total": 0,
      "input": 0,
      "output": 0,
      "cacheRead": 0,
      "cacheWrite": 0,
      "uncachedInput": 0,
      "records": 0,
      "cacheHitRate": null
    },
    "models": [],
    "projects": [],
    "sessions": [{
      "key": "project · session-id",
      "title": "Readable session title",
      "project": "project",
      "sessionId": "session-id",
      "input": 0,
      "output": 0,
      "cacheRead": 0,
      "cacheWrite": 0,
      "uncachedInput": 0,
      "records": 0,
      "cacheHitRate": null
    }],
    "daily": [],
    "dailyByModel": {
      "model-id": []
    },
    "hours": [],
    "filesScanned": 0,
    "parseErrors": 0,
    "coverageStartAt": null,
    "coverageEndAt": null
  }]
}
```

- `source` is one of `workbuddy`, `workbuddy-ai`, `codebuddy-cli`, or
  `codebuddy-ide`. The four sources are returned independently and never mixed;
  domestic and international WorkBuddy usage must not be summed into a single
  source.
- `summary.input` includes the provider-reported input total, including cached
  input. `summary.uncachedInput = input - cacheRead` (saturating at zero).
- `cacheWrite` accepts only explicit provider write aliases:
  `cache_write_input_tokens`, `cacheWriteInputTokens`,
  `cache_creation_input_tokens`, and `prompt_cache_write_tokens`. When the
  selected usage object has no positive write value, the decoder may inspect
  the other usage objects and `providerData.rawUsage` for those aliases only.
  `prompt_cache_miss_tokens` is not a write alias; it represents uncached input
  and must remain part of `uncachedInput`.
- `summary.cacheHitRate` is `cacheRead / input` when input is positive;
  otherwise it is `null`.
- Dashboard total is derived from `input + output + cacheWrite`; composition
  uses `cacheRead`, `uncachedInput`, `output`, and `cacheWrite`.
- `models`, `projects`, `sessions`, `daily`, and `hours` contain `{ key, total,
  input, output, cacheRead, cacheWrite, uncachedInput, records }` projections,
  sorted by descending `total`.
- `dailyByModel` is an object keyed by the same model labels exposed in
  `models`; each value is a daily projection array using the same totals shape
  as `daily`. It is additive and may be absent in older responses. Consumers
  must keep the aggregate `daily` series as the default and only expose a model
  filter when `dailyByModel` is available; they must never fake a model trend
  by reusing the aggregate series.
- Session projections additionally expose optional `title` plus safe `project`
  and `sessionId` labels. `aiTitle` is the preferred title; the latest non-empty
  `summary` in the same JSONL file is the fallback. Title metadata is collected
  before applying the usage time cutoff, so an older title can label in-range
  usage. The JSONL file remains the session identity boundary: equal titles
  never merge two files or change their token totals.
- Project labels are reduced to a safe basename. Absolute path-like encoded
  project keys are not returned. Message bodies, arguments, raw paths, and
  authentication data are never included.
- Records under a directory named exactly `subagents` are excluded. A record is
  counted once using usage precedence `message.usage`, then
  `providerData.usage`, then top-level `usage`; `providerData.rawUsage` only
  supplements cache-write fields absent from the selected usage object.
- Copied/forked sessions replay the parent history, including usage records with
  their original timestamps, into their own JSONL file. The decoder therefore
  processes files oldest-first (by file mtime) and skips any record whose
  fingerprint was already seen in an earlier file. The fingerprint is
  `(timestamp, input, output, cacheRead, cacheWrite, model)`. A record without a
  timestamp cannot be fingerprinted and is counted as before. Usage stays
  attributed to the original session; a fork only contributes records it
  produced itself.
- The fingerprint set is scoped to one `source_from_roots` call, not to a
  directory. All scan roots of the same variant therefore share one set: a
  record replayed across `~/.workbuddy-ai/projects` and
  `~/.workbuddy-ai/sessions` counts once. Two different sources never share a
  set, so one source can never absorb another's records.

#### Request detail rows (`requests`, CodeBuddy CLI only)

- The `codebuddy-cli` source additionally returns `requests`: one row per model
  call, newest first, capped at `REQUEST_WINDOW` (500).
- `workbuddy` and `codebuddy-ide` never emit the key. Their bodies stay
  byte-for-byte unchanged, so consumers must read a missing `requests` as "not
  available" rather than as an empty log.
- Row shape: `{ timestamp, model, project, sessionId, title, input, output,
  cacheRead, cacheWrite, total }` with `total = input + output + cacheWrite`,
  produced by the same helper as `summary.total` — never by a second formula.
- Rows come from the same decode / cutoff / fingerprint-dedupe / `subagents`
  pipeline as the aggregates. A parallel parse path is forbidden: a detail row
  must never disagree with the totals it sits next to.
- A usage record without a `timestamp` cannot be ordered. It still counts in the
  aggregates but never appears in `requests`, so `requests.length` may be lower
  than `summary.records` by those rows; the UI must not promise equality.
- `title` is file-scoped: `aiTitle` wins, the latest non-empty `summary` is the
  fallback, and it is backfilled after the whole JSONL file is read because the
  title event may follow the usage records.
- Scanning holds at most `REQUEST_COLLECT_LIMIT` rows; on overflow it trims to
  the newest `REQUEST_WINDOW` with a stable descending timestamp sort. The
  retained set is always the globally newest window, independent of file order
  (mtime-sorted) or row-arrival order.
- Privacy: a row carries only the model label, the `cwd` basename, the session
  id, the file-scoped title, and token counts. Message bodies, tool arguments,
  raw paths, and authentication data are never included.

### 4. Validation & Error Matrix

| Condition | Required behavior |
|---|---|
| `days=7`, `30`, or `90` | Apply one shared millisecond cutoff to all sources. |
| Missing/invalid `days` | Scan complete history and return `rangeDays: null`. |
| Missing source directory | Return an empty source, not an API error. |
| Missing `~/.workbuddy-ai` roots (international client not installed) | Return an empty `workbuddy-ai` source; the UI shows an identifiable empty state distinct from the domestic source. |
| Invalid JSONL line | Skip the line and increment `parseErrors`. |
| Missing timestamp or outside cutoff | Do not aggregate the record. |
| Missing input usage field | Ignore the record as non-usage content. |
| Zero input with a valid usage field | Count the record; hit rate remains `null`. |
| Explicit cache-write aliases are missing or zero | Return `cacheWrite: 0`; the UI explains that this source has no explicit write quantity. |
| Positive write alias exists only in another usage object or `rawUsage` | Use that positive explicit value without counting input/output twice. |
| Only `prompt_cache_miss_tokens` is positive | Keep `cacheWrite: 0`; derive uncached input from `input - cacheRead`. |
| `subagents` directory | Do not scan files below that directory. |
| IDE `messages` / `check-point` / `backups` / `Public` | Do not scan those directories. |
| IDE conversation index without `requests` | Skip the file; do not treat workspace indexes as usage. |
| IDE request with `inputTokens` present | Count once using `cacheTokens` as cache read and `cachedWriteTokens` as cache write. |
| Title event before/after usage | Associate the latest non-empty title in the same file. |
| Title timestamp before cutoff | Keep the title when that file has in-range usage. |
| Missing/blank title | Return `title: null`; the UI uses a redacted session fallback. |
| Equal titles in different files | Return separate session groups with stable unique keys. |
| Older response without `dailyByModel` | Keep the aggregate daily trend and do not expose model-specific options. |
| Selected model has no dated usage | Return an empty model trend; never fall back to the aggregate series under that model label. |
| Copied/forked session replays the same usage record in another file | Count it once, attributed to the earliest-mtime file; skip later copies of the same fingerprint. |
| Same fingerprint appears in two scan roots of one variant | Count it once; the two WorkBuddy AI roots share one fingerprint set. |
| Record has no timestamp | Count it as before; it cannot be fingerprinted and must not be dropped. |
| File mtimes tie at millisecond resolution | Ordering falls back to directory iteration; the contract still requires oldest-first by mtime, so fixtures must pin mtimes instead of relying on write order. |
| Non-CLI source | Omit `requests` entirely; never fill it from another source's window. |
| Detail row candidate | Apply the same `seen` fingerprint as the aggregates, so copied/forked session replay cannot duplicate a row. |
| Usage record without a timestamp | Count it in the aggregates; exclude it from `requests`. |
| Candidates exceed `REQUEST_WINDOW` | Return only the newest 500 rows and let the UI disclose the truncation. |
| `aiTitle` appears after the usage records | Backfill that title onto every detail row of the same file. |
| Title event exists but the file has no in-range usage | Emit no detail row for it. |

### 5. Good / Base / Bad Cases

- Good: a record with `message.usage.input_tokens` and aliases for output/cache
  fields is normalized and appears once in all applicable groups.
- Good cache write: `message.usage` supplies canonical input/output while
  `providerData.rawUsage.prompt_cache_write_tokens` supplies a positive write;
  the record still counts once and preserves that explicit write.
- Base: an empty local source returns zero totals, empty groups, and no error.
- Base cache write: write aliases are present but all zero, so the response
  keeps `cacheWrite: 0` and never invents a write amount.
- Bad: a record containing only provider metadata or message text contributes no
  tokens; malformed JSON contributes only to `parseErrors`.
- Bad cache write: mapping `prompt_cache_miss_tokens` to `cacheWrite` falsely
  labels newly computed input as cache creation.
- Good title: a file containing `aiTitle`, `summary`, and usage returns the
  latest non-empty `aiTitle` without changing usage totals.
- Base title: a session without title metadata returns `title: null` and safe
  project/session labels.
- Bad title: never derive a title from message content, tool arguments, output,
  raw paths, or authentication data.
- Good detail: a CLI fixture with three dated usage records returns three
  `requests` rows, newest first, each with `total = input + output + cacheWrite`,
  and their `total` sum stays inside `summary.total`.
- Base detail: a source scanned with `detail = false`, or a source with no usage,
  returns no `requests` key at all.
- Bad detail: filling `requests` for every source, or rebuilding rows with a
  second parser, makes the detail list disagree with the dashboard numbers it is
  displayed next to.

### 6. Tests Required

- Unit tests assert usage precedence, field aliases, raw cache-write fallback,
  cross-object positive cache-write fallback, miss/write separation,
  zero-input handling, saturating uncached input, and
  `total = input + output + cacheWrite`.
- Fixture tests assert `subagents` exclusion, one-record counting, malformed
  line accounting, project basename redaction, and cutoff filtering.
- Fixture tests assert that each dated usage record contributes once to
  `daily` and once to its matching `dailyByModel[model]` series, with no
  cross-model leakage.
- Session fixture tests assert file-scoped title association, `aiTitle` over
  `summary`, title events before and after usage, titles older than the usage
  cutoff, untitled fallback, and equal-title sessions remaining distinct.
- HTTP/Tauri compile tests must continue to pass after the optional `days`
  signature change.
- Frontend/browser checks must assert source tabs, range selection, summary,
  trend, composition, heatmap, rankings, empty/error states, and no horizontal
  overflow at narrow viewport widths.
- Fixture tests assert CodeBuddy IDE conversation indexes are aggregated, message
  body files are ignored, titles/models come from the workspace index, and the
  shared cutoff applies to `startedAt`.
- Source tests assert the four sources are keyed independently, that each
  WorkBuddy variant scans its own roots, that a missing AI root yields an empty
  `workbuddy-ai` source, and that domestic and international WorkBuddy totals
  are never combined (including the empty-state distinction).
- Dedup fixture tests assert a copied/forked session's replayed record counts
  once while the fork's own new records still count, and that two roots of the
  same variant share one fingerprint set (a record replayed across roots counts
  once, and the second root is still scanned).
- Detail fixtures must assert: the `requests` key is present only when
  `detail = true`; row `total` uses the shared helper; rows sort newest-first
  regardless of write order; the window keeps the globally newest `REQUEST_WINDOW`
  rows after a trim, for out-of-order arrivals too; a copied/forked session
  contributes one row; a record without a timestamp stays in the aggregates but
  out of `requests`; and an `aiTitle` placed after the usage records still labels
  every row of that file.

### 7. Wrong vs Correct

#### Wrong

```rust
// Counts providerData and message usage as two independent records.
total.add(usage(provider_data));
total.add(usage(message));
```

#### Correct

```rust
// Select one canonical usage object, then aggregate it once.
let selected = message_usage.or(provider_usage).or(top_level_usage)?;
total.add(normalize(selected));
```

#### Wrong

```rust
// Cache misses are newly computed input, not cache creation.
write = field(raw_usage, &["prompt_cache_miss_tokens"]).unwrap_or(0);
```

#### Correct

```rust
// Only explicit cache-write aliases may populate cacheWrite.
write = positive_field(raw_usage, CACHE_WRITE_KEYS).unwrap_or(0);
```

#### Wrong

```rust
// Applying cutoff first drops older title-only events, and keying by title
// merges unrelated sessions that happen to share a label.
if timestamp(&event)? < cutoff { continue; }
sessions.entry(event["aiTitle"].to_string()).or_default();
```

#### Correct

```rust
// Collect file metadata before cutoff; only usage is time-filtered. Preserve
// file identity and attach the preferred title to that file's aggregate.
collect_title_metadata(&event);
if usage_is_in_range(&event, cutoff) {
    session_totals.add(normalize_usage(&event)?);
}
```

#### Wrong

```rust
```rust
// Scoping the fingerprint set to a root silently re-counts a record that a
// copied session replayed into another root of the same variant.
for root in roots {
    let mut seen = HashSet::new();
    for path in files(root) {
        aggregate(path, &mut seen);
    }
}
```

```rust
// A second walk over the logs for the detail list re-derives dedupe and cutoff,
// so the rows can disagree with the totals rendered beside them. Emitting the
// key unconditionally also breaks the sources that must stay unchanged.
let rows = walk_logs_again_for_details();
value["requests"] = json!(rows);
```
```

#### Correct

```rust
```rust
// One fingerprint set per source: shared by every root of the variant, never
// shared between sources. Files are processed oldest-first by mtime.
let mut seen = HashSet::new();
paths.sort_by_key(|(_, path)| mtime(path));
for (root, path) in &paths {
    aggregate(root, path, &mut seen);
}
```

```rust
// Detail rows ride the same pass, after the shared fingerprint check, and the
// key exists only for the source that asked for detail.
if let Some(ts) = timestamp(&event) {
    if seen.insert(fingerprint(&event, usage)) {
        file_rows.push(RequestRow { timestamp: ts, usage, ..row_labels(&event) });
    }
}
// after the file is read, backfill the file-scoped title
if detail {
    value["requests"] = json!(newest_window(file_rows));
}
```
}
```
