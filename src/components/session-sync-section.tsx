import { useEffect, useState } from "react";
import { CircleAlert, Loader2, RotateCw } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";
import { accountVariant } from "@/lib/variant";
import type {
  AccountMeta,
  SessionLinkPreviewGroup,
  SessionLinksPreview,
  SessionSyncMode,
  SessionSyncSelection,
  SessionSyncVerdict,
} from "@/lib/types";

interface Props {
  /** 目标账号（来源账号身份由后端从该档位登录态读取，前端不传） */
  account: AccountMeta | null;
  /** 弹窗是否打开：打开时拉取一次预览 */
  open: boolean;
  /** 切换进行中：禁止继续交互 */
  disabled?: boolean;
  /** 勾选结果变化：父组件据此提交 `syncSelections`，并用于结果反馈里回显会话名 */
  onChange: (state: { selections: SessionSyncSelection[]; groups: SessionLinkPreviewGroup[] }) => void;
}

/** 判定结果的中文标签（与 core 的 verdict 一一对应）。 */
const VERDICT_LABEL: Record<SessionSyncVerdict, string> = {
  fastForward: "可快进",
  diverge: "内容冲突",
  ahead: "目标已变化",
  identical: "内容一致",
  unknown: "需手动处理",
};

const VERDICT_BADGE: Record<SessionSyncVerdict, "success" | "warning" | "outline" | "secondary"> = {
  fastForward: "success",
  diverge: "warning",
  ahead: "warning",
  identical: "secondary",
  unknown: "outline",
};

/** 可勾选的模式：判定只允许一个模式，取后端给出的第一个。 */
function primaryMode(group: SessionLinkPreviewGroup): SessionSyncMode | null {
  return group.availableModes.length > 0 ? group.availableModes[0] : null;
}

function isActionable(group: SessionLinkPreviewGroup): boolean {
  return primaryMode(group) !== null && Boolean(group.previewToken);
}

/**
 * 组装提交给后端的同步选择：只有「用户勾选 + 后端给出模式与预览凭据」的组才发送。
 * 前端不推断模式，也不为禁选项补默认值。
 */
function buildSelections(
  groups: SessionLinkPreviewGroup[],
  checked: Set<string>,
): SessionSyncSelection[] {
  return groups.flatMap((group) => {
    if (!checked.has(group.groupId)) return [];
    const mode = primaryMode(group);
    if (!mode || !group.previewToken) return [];
    return [{ groupId: group.groupId, previewToken: group.previewToken, mode }];
  });
}

/**
 * 切号弹窗内的「会话同步」区块（prd R3 / R6，不新增独立弹窗）。
 *
 * - 默认勾选与可选模式全部来自后端：`defaultChecked` 为 true 才预先勾选，
 *   `availableModes` 为空（identical / ahead / unknown / 预览凭据不可用）一律禁选。
 * - `diverge` 默认不勾，需用户显式选择覆盖，并展示「覆盖会替换目标全文」与目标独有记录数。
 * - 文案统一称「记录数」，不把 JSONL 行数叫消息数。
 */
export function SessionSyncSection({ account, open, disabled, onChange }: Props) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [preview, setPreview] = useState<SessionLinksPreview | null>(null);
  /** 用户逐项勾选状态；仅在可勾选项上生效。 */
  const [checked, setChecked] = useState<Set<string>>(new Set());
  /** 手动重试计数：用于「重新检查」。 */
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    if (!open || !account) {
      setPreview(null);
      setError("");
      setChecked(new Set());
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError("");
    api
      .sessionLinksPreview(account.id, accountVariant(account))
      .then((res) => {
        if (cancelled) return;
        setPreview(res);
        // 默认勾选值来自后端 defaultChecked，前端不扩大权限。
        const defaults = new Set(
          res.groups.filter((group) => group.defaultChecked && isActionable(group)).map((g) => g.groupId),
        );
        setChecked(defaults);
        onChange({ selections: buildSelections(res.groups, defaults), groups: res.groups });
      })
      .catch((e) => {
        if (cancelled) return;
        setPreview(null);
        setError(api.asError(e));
        onChange({ selections: [], groups: [] });
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
    // 预览拉取只看「弹窗开关 / 目标账号 / 手动重试」；onChange 只做状态回写，
    // 不进依赖，否则父组件每次重渲染都会重新拉预览。
  }, [open, account, reloadToken]);

  function toggleGroup(group: SessionLinkPreviewGroup, next: boolean) {
    const updated = new Set(checked);
    if (next) updated.add(group.groupId);
    else updated.delete(group.groupId);
    setChecked(updated);
    onChange({ selections: buildSelections(preview?.groups ?? [], updated), groups: preview?.groups ?? [] });
  }

  // 国际版能力判定不通过：整块隐藏（后端执行时仍会强制检查能力）。
  if (preview && (!preview.supported || preview.storeStatus === "unsupported")) return null;

  const groups = preview?.groups ?? [];
  const actionable = groups.filter(isActionable);
  const selectedCount = groups.filter((g) => checked.has(g.groupId) && isActionable(g)).length;
  const summary = summarize(groups);
  // 首次渲染（effect 未跑完）与请求中同样按「加载中」呈现，避免闪一帧空态。
  const pending = loading || (!preview && !error);

  return (
    <section className="space-y-2" aria-label="会话同步">
      <div className="flex items-start justify-between gap-3 rounded-md border px-3 py-2.5">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium">同步关联会话的正文</div>
          <div className="text-xs text-muted-foreground">
            {error
              ? "无法检查关联会话，暂不能同步"
              : pending
                ? "正在检查关联会话…"
                : preview?.storeStatus === "unavailable"
                  ? "会话关联记录不可用，本次不会同步"
                  : groups.length === 0
                    ? "没有可同步的关联会话"
                    : summary}
          </div>
        </div>
        {!pending && (error || preview?.storeStatus === "unavailable") && (
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={disabled}
            onClick={() => setReloadToken((token) => token + 1)}
          >
            <RotateCw />
            重新检查
          </Button>
        )}
      </div>

      {pending && (
        <div className="flex items-center gap-2 px-1 py-2 text-xs text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" />
          加载关联会话…
        </div>
      )}

      {error && (
        <Alert variant="destructive" className="min-w-0">
          <CircleAlert />
          <AlertTitle>无法检查会话关联</AlertTitle>
          <AlertDescription className="min-w-0 break-all">{error}</AlertDescription>
        </Alert>
      )}

      {!pending && !error && preview?.storeStatus === "unavailable" && (
        <Alert variant="warning" className="min-w-0">
          <CircleAlert />
          <AlertTitle>会话关联记录不可用</AlertTitle>
          <AlertDescription className="min-w-0 break-all">
            {`${preview.storeError ?? "原因未知"}；本次不会同步任何会话，请手动处理后再试。`}
          </AlertDescription>
        </Alert>
      )}

      {!pending && !error && preview?.storeStatus === "missing" && (
        <p className="px-1 py-1 text-xs text-muted-foreground">
          还没有会话关联记录：先用「复制会话到目标账号」复制一次，之后切号就能在这里同步双方的后续内容。
        </p>
      )}

      {!pending && !error && preview?.storeStatus === "ready" && groups.length === 0 && (
        <p className="px-1 py-1 text-xs text-muted-foreground">
          当前账号与目标账号之间没有共同参与的关联会话（只处理双方都有的组，不涉及其他账号）。
        </p>
      )}

      {!pending && !error && groups.length > 0 && (
        <div className="space-y-2">
          <p className="px-1 text-xs text-muted-foreground">
            默认只勾选「目标未偏离共同基线」的快进项；覆盖目标全文需你手动选择。
            {actionable.length > 0 && selectedCount === 0 && " 当前未勾选任何会话。"}
          </p>
          {groups.map((group) => (
            <GroupRow
              key={group.groupId}
              group={group}
              checked={checked.has(group.groupId) && isActionable(group)}
              disabled={disabled}
              onToggle={(next) => toggleGroup(group, next)}
            />
          ))}
        </div>
      )}
    </section>
  );
}

/** 区块标题下的一行摘要：只报数量，不把「零条损失」当作安全暗示。 */
function summarize(groups: SessionLinkPreviewGroup[]): string {
  const parts: string[] = [];
  const count = (verdict: SessionSyncVerdict) =>
    groups.filter((group) => group.verdict === verdict).length;
  if (count("fastForward") > 0) parts.push(`${count("fastForward")} 个可快进（已默认勾选）`);
  if (count("diverge") > 0) parts.push(`${count("diverge")} 个内容冲突（需手动选择覆盖）`);
  if (count("ahead") > 0) parts.push(`${count("ahead")} 个仅目标有变化（本次不同步）`);
  if (count("identical") > 0) parts.push(`${count("identical")} 个内容一致（无需写入）`);
  if (count("unknown") > 0) parts.push(`${count("unknown")} 个需手动处理`);
  return parts.join("；");
}

function GroupRow({
  group,
  checked,
  disabled,
  onToggle,
}: {
  group: SessionLinkPreviewGroup;
  checked: boolean;
  disabled?: boolean;
  onToggle: (next: boolean) => void;
}) {
  const mode = primaryMode(group);
  const canSelect = isActionable(group);
  const overwrite = mode === "overwrite";

  return (
    // 整行是一块可点区域：label 包住 Checkbox（该按钮即 label 的控制元素），
    // 点标题/说明/记录数都能切换勾选；禁选项不给 cursor-pointer。
    <label
      className={cn(
        "flex gap-2.5 rounded-md border px-3 py-2",
        canSelect && "cursor-pointer hover:bg-accent/50",
        !canSelect && "bg-muted/30",
      )}
    >
      <Checkbox
        className="mt-0.5"
        checked={checked}
        disabled={disabled || !canSelect}
        onCheckedChange={(state) => onToggle(state === true)}
        aria-label={`同步会话 ${group.title || "(无标题)"}`}
      />
      <span className="block min-w-0 flex-1 space-y-0.5 text-sm">
        <span className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate font-medium" title={group.title}>
            {group.title || "(无标题)"}
          </span>
          <Badge variant={VERDICT_BADGE[group.verdict]} className="shrink-0 text-[10px]">
            {VERDICT_LABEL[group.verdict]}
          </Badge>
        </span>
        {group.cwd && (
          <span className="block truncate text-xs text-muted-foreground" title={group.cwd}>
            {group.cwd}
          </span>
        )}
        <span className="block text-xs text-muted-foreground">{group.reason}</span>
        <span className="block text-xs text-muted-foreground">
          {`记录数：来源 ${group.recordCount.source} / 目标 ${group.recordCount.target}`}
          {group.recordCount.baseline !== null && ` / 共同基线 ${group.recordCount.baseline}`}
          {group.extraB > 0 && ` · 目标独有 ${group.extraB}`}
        </span>
        {overwrite && (
          <span className="mt-1 block rounded-md border border-amber-500/30 bg-amber-500/10 px-2 py-1.5 text-xs text-amber-900 dark:text-amber-200">
            <span className="block font-medium">勾选即表示你选择「覆盖目标全文」</span>
            <span className="mt-0.5 block">
              {`覆盖会用来源正文替换目标全文（目标独有 ${group.extraB} 条记录），目标的会话 id 与标题保留。记录数相同或目标独有记录为 0 都不代表顺序无损。`}
            </span>
          </span>
        )}
        {!canSelect && group.verdict === "ahead" && (
          <span className="block text-xs text-muted-foreground">
            仅目标有变化，本次不改动目标，下次切号会重新提示。
          </span>
        )}
      </span>
    </label>
  );
}
