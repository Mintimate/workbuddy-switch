import { useEffect, useState } from "react";
import { Link2, Loader2, RotateCw } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
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
  /** 预览状态上报：父组件用于 tab 徽标与常驻提示，避免把错误藏进 tab 里 */
  onMetaChange?: (meta: SessionLinksMeta) => void;
}

/** 关联会话区块对父组件暴露的状态（tab 徽标 / 常驻提示用）。 */
export interface SessionLinksMeta {
  /** 该区块是否应渲染（国际版或存储不支持时为 false，父组件不渲染本 tab） */
  available: boolean;
  /** 可同步的会话数量（tab 徽标数字，0 时父组件不显示徽标） */
  groupCount: number;
  /** 预览请求失败的原因；非空时父组件在 tab 之上常驻提示 */
  error: string;
  /** 关联存储状态；unavailable 时父组件常驻提示原因 */
  storeStatus: SessionLinksPreview["storeStatus"] | null;
  storeError: string;
}

/** 判定结果的中文标签（与 core 的 verdict 一一对应，只表达状态，动作交给摘要句）。 */
const VERDICT_LABEL: Record<SessionSyncVerdict, string> = {
  fastForward: "有新内容",
  diverge: "两边都改过",
  ahead: "目标账号有更新",
  identical: "两边一致",
  unknown: "无法确认",
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

/** 卡片摘要句：一句人话说清发生了什么；条数与原始原因收进「查看详情」。 */
function summarySentence(group: SessionLinkPreviewGroup, targetLabel: string): string {
  switch (group.verdict) {
    case "fastForward":
      return `当前账号有 ${group.extraA} 条新内容，可以直接同步。`;
    case "diverge":
      return `两边都有改动；同步会替换「${targetLabel}」的完整内容。`;
    case "ahead":
      return `「${targetLabel}」中的此会话已有新内容，本次保留，不同步。`;
    case "identical":
      return "两边内容一致，无需同步。";
    case "unknown":
      return "暂时无法确认两边内容，本次不会同步。";
  }
}

const RECORD_COUNT_HINT = "按会话内容的条数统计，不是对话轮数；条数相同也不代表内容顺序完全一致。";

/**
 * 切号弹窗「关联会话」tab 的内容：说明卡 + 会话卡片（判定徽标 + 一句摘要 + 折叠详情）。
 *
 * - 默认勾选与可选模式全部来自后端：`defaultChecked` 为 true 才预先勾选，
 *   `availableModes` 为空（identical / ahead / unknown / 预览凭据不可用）一律禁选。
 * - `diverge` 默认不勾，需用户显式选择覆盖；勾选后详情强制展开并显示覆盖警告。
 * - 错误与存储不可用由父组件在 tab 之上常驻提示，本组件只保留对应的空态与重试入口。
 */
export function SessionSyncSection({ account, open, disabled, onChange, onMetaChange }: Props) {
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

  const groups = preview?.groups ?? [];
  // 国际版能力判定不通过：整块不可用（后端执行时仍会强制检查能力）。
  const unsupported = Boolean(preview && (!preview.supported || preview.storeStatus === "unsupported"));
  const targetLabel = account?.nickname || account?.email || account?.uid || "目标账号";

  // 状态上报：父组件据此渲染 tab 徽标与常驻提示。
  useEffect(() => {
    onMetaChange?.({
      available: !unsupported,
      groupCount: groups.length,
      error,
      storeStatus: preview?.storeStatus ?? null,
      storeError: preview?.storeError ?? "",
    });
    // onMetaChange 只做状态回写，不进依赖。
  }, [unsupported, groups.length, error, preview?.storeStatus, preview?.storeError]);

  function toggleGroup(group: SessionLinkPreviewGroup, next: boolean) {
    const updated = new Set(checked);
    if (next) updated.add(group.groupId);
    else updated.delete(group.groupId);
    setChecked(updated);
    onChange({ selections: buildSelections(groups, updated), groups });
  }

  if (unsupported) return null;

  const pending = loading || (!preview && !error);
  const storeUnavailable = preview?.storeStatus === "unavailable";

  return (
    <section className="space-y-3" aria-label="关联会话">
      <div className="flex items-start gap-3 rounded-md border bg-muted/30 px-3 py-3">
        <span className="flex size-8 shrink-0 items-center justify-center rounded-md border bg-background text-muted-foreground">
          <Link2 className="size-4" />
        </span>
        <div className="min-w-0 space-y-0.5">
          <div className="text-sm font-medium">什么是关联会话？</div>
          <p className="text-xs text-muted-foreground">
            通过本工具复制到其他账号的会话，会自动建立关联。切换账号时，可以把当前账号的后续内容同步到对应会话。
          </p>
        </div>
      </div>

      {pending && (
        <div className="flex items-center gap-2 px-1 py-2 text-xs text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" />
          正在检查会话…
        </div>
      )}

      {!pending && (error || storeUnavailable) && (
        <div className="flex items-center justify-between gap-3 rounded-md border px-3 py-2.5">
          <span className="min-w-0 flex-1 text-xs text-muted-foreground">
            {error ? "暂时无法检查会话，本次不能同步" : "同步记录不可用，本次不会同步"}
          </span>
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
        </div>
      )}

      {!pending && !error && preview?.storeStatus === "missing" && (
        <p className="px-1 py-1 text-xs text-muted-foreground">
          还没有可以同步的会话：先在「复制会话」里复制一次，之后切换回来就能在这里同步新内容。
        </p>
      )}

      {!pending && !error && !storeUnavailable && preview?.storeStatus === "ready" && groups.length === 0 && (
        <p className="px-1 py-1 text-xs text-muted-foreground">
          这两个账号还没有共同复制过的会话（只处理双方都有的，不涉及其他账号）。
        </p>
      )}

      {!pending && !error && groups.length > 0 && (
        <div className="space-y-2">
          {groups.map((group) => (
            <SessionLinkCard
              key={group.groupId}
              group={group}
              targetLabel={targetLabel}
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

/** 会话卡片：标题 + 判定徽标 + 一句摘要 + 折叠详情（条数 / 原因 / 覆盖警告）。 */
function SessionLinkCard({
  group,
  targetLabel,
  checked,
  disabled,
  onToggle,
}: {
  group: SessionLinkPreviewGroup;
  targetLabel: string;
  checked: boolean;
  disabled?: boolean;
  onToggle: (next: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const mode = primaryMode(group);
  const canSelect = isActionable(group);
  const overwrite = mode === "overwrite";
  // 勾选覆盖时详情必须展开并显示警告，不允许把高风险提示藏在折叠后。
  const forceOpen = overwrite && checked;
  const expanded = forceOpen || open;
  const title = group.title || "(无标题)";

  return (
    <Collapsible
      open={expanded}
      onOpenChange={(next) => {
        if (!forceOpen) setOpen(next);
      }}
      className={cn(
        "space-y-1 rounded-md border px-3 py-2.5",
        canSelect && "hover:bg-accent/30",
        !canSelect && "bg-muted/30",
      )}
    >
      <div className="flex items-start gap-2.5">
        {canSelect && (
          <Checkbox
            className="mt-0.5"
            checked={checked}
            disabled={disabled}
            onCheckedChange={(state) => onToggle(state === true)}
            aria-label={`同步会话 ${title}`}
          />
        )}
        <div className="min-w-0 flex-1 space-y-0.5">
          <div className="flex items-center gap-2">
            <span className="min-w-0 flex-1 truncate text-sm font-medium" title={group.title}>
              {title}
            </span>
            <Badge variant={VERDICT_BADGE[group.verdict]} className="shrink-0 text-[10px]">
              {VERDICT_LABEL[group.verdict]}
            </Badge>
          </div>
          <p className="text-xs text-muted-foreground">{summarySentence(group, targetLabel)}</p>
        </div>
        <CollapsibleTrigger asChild>
          <Button
            variant="ghost"
            size="sm"
            className="shrink-0 text-xs text-muted-foreground"
            disabled={forceOpen}
            aria-expanded={expanded}
          >
            {expanded && !forceOpen ? "收起详情" : "查看详情"}
          </Button>
        </CollapsibleTrigger>
      </div>

      <CollapsibleContent className="space-y-1.5 pt-1 text-xs text-muted-foreground">
        {group.cwd && (
          <span className="block truncate" title={group.cwd}>
            {group.cwd}
          </span>
        )}
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="block w-fit cursor-help" tabIndex={0}>
              {`内容条数：当前账号 ${group.recordCount.source} 条 · 目标账号 ${group.recordCount.target} 条`}
              {group.recordCount.baseline !== null && ` · 上次一致 ${group.recordCount.baseline} 条`}
              {group.extraB > 0 && ` · 目标账号独有 ${group.extraB} 条`}
            </span>
          </TooltipTrigger>
          <TooltipContent side="top" className="max-w-xs">
            {RECORD_COUNT_HINT}
          </TooltipContent>
        </Tooltip>
        <span className="block">{group.reason}</span>
        {overwrite && (
          <span className="block rounded-md border border-amber-500/30 bg-amber-500/10 px-2 py-1.5 text-amber-900 dark:text-amber-200">
            <span className="block font-medium">勾选后会替换目标账号的完整内容</span>
            <span className="mt-0.5 block">
              {`会用当前账号的内容替换目标账号这条会话的全部内容（其中目标账号独有的 ${group.extraB} 条会被替换掉），会话标题保持不变。条数相同也不代表内容顺序完全一致；完成后无法在本工具中撤销。`}
            </span>
          </span>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}
