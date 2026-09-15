import { useEffect, useState } from "react";
import { ExternalLink } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { DEFAULT_VARIANT, variantAppName, variantLabel } from "@/lib/variant";
import type { AccountMeta, WbVariant } from "@/lib/types";
import { useAccountsStore } from "@/stores/accounts";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 登录目标档位；缺省国内版（零回归）。 */
  variant?: WbVariant;
}

/** 登录链路文案按档位区分：国内版是扫码授权，国际版只有浏览器 Web 登录授权（无二维码）。 */
const LOGIN_COPY = {
  cn: {
    title: "OAuth 扫码登录",
    description: (appName: string) =>
      `在浏览器中打开验证链接，扫码授权后将自动采集 ${appName} 账号并入库。`,
    start: "开始扫码登录",
    waiting: "正在等待扫码授权，请在浏览器完成操作…",
  },
  ai: {
    title: "OAuth Web 登录",
    description: (appName: string) =>
      `在浏览器中打开验证链接，完成 Web 登录授权后将自动采集 ${appName} 账号并入库。`,
    start: "开始 Web 登录",
    waiting: "请在浏览器中完成 Web 登录授权，正在等待授权结果…",
  },
} as const;

/** OAuth 登录采集：发起 → 打开浏览器 → 轮询采集结果 → 入库。 */
export function OAuthLoginDialog({ open, onOpenChange, variant = DEFAULT_VARIANT }: Props) {
  const reconcileAccounts = useAccountsStore((s) => s.reconcileAccounts);
  const appName = variantAppName(variant);
  const copy = LOGIN_COPY[variant];

  const [busy, setBusy] = useState(false);
  const [loginId, setLoginId] = useState<string | null>(null);
  const [uri, setUri] = useState("");
  const [error, setError] = useState("");
  const [result, setResult] = useState<AccountMeta | null>(null);

  // 打开时重置
  useEffect(() => {
    if (open) {
      setBusy(false);
      setLoginId(null);
      setUri("");
      setError("");
      setResult(null);
    }
  }, [open]);

  // 关闭或卸载对话框时收起应用内授权窗口，避免留下孤儿窗口。
  useEffect(() => {
    if (!open || variant !== "ai") return;
    return () => closeAuthWindow(variant);
  }, [open, variant]);

  // 轮询采集结果
  useEffect(() => {
    if (!loginId) return;
    let timer: number | undefined;
    let cancelled = false;

    const poll = async () => {
      try {
        const res = await api.oauthStatus(loginId);
        if (res.done) {
          // 成功或失败都以轮询结果为准收起授权窗口，不猜服务端的跳转地址。
          closeAuthWindow(variant);
          if (res.result) {
            await reconcileAccounts();
            if (!cancelled) setResult(res.result);
          } else if (!cancelled) {
            setError(res.error || "登录失败");
          }
          if (timer !== undefined) window.clearInterval(timer);
          return;
        }
        timer = window.setTimeout(poll, 1500);
      } catch (e) {
        if (!cancelled) setError(api.asError(e));
        if (timer !== undefined) window.clearInterval(timer);
      }
    };
    poll();

    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [loginId, reconcileAccounts, variant]);

  async function start() {
    setBusy(true);
    setError("");
    try {
      const res = await api.oauthStart(variant);
      setLoginId(res.loginId);
      setUri(res.verificationUri);
      // 按当前宿主能力与档位打开验证页
      await openAuthPage(res.verificationUri, variant);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{copy.title}（{variantLabel(variant)}）</DialogTitle>
          <DialogDescription>{copy.description(appName)}</DialogDescription>
        </DialogHeader>

        {!loginId && !result && (
          <div className="space-y-3">
            <Button onClick={start} disabled={busy} className="w-full">
              {busy ? "正在发起登录…" : copy.start}
            </Button>
          </div>
        )}

        {loginId && !result && (
          <div className="space-y-3">
            <Alert>
              <ExternalLink className="size-4" />
              <AlertDescription className="break-all">
                <a
                  href={uri}
                  target="_blank"
                  rel="noreferrer"
                  className="text-primary underline-offset-2 hover:underline"
                  onClick={(e) => {
                    // WebUI 直接使用浏览器默认链接行为，确保即使自动弹窗被拦截
                    // 也能通过用户点击打开验证页。
                    if (api.isWebui()) return;
                    e.preventDefault();
                    void openAuthPage(uri, variant);
                  }}
                >
                  {uri}
                </a>
              </AlertDescription>
            </Alert>
            <p className="text-sm text-muted-foreground">
              {copy.waiting}
            </p>
            {api.isWebui() && variant === "ai" && (
              <p className="text-sm text-muted-foreground">
                WebUI 通道无法隔离浏览器会话；若浏览器已登录 workbuddy.ai，请改用无痕窗口打开上方链接再授权。
              </p>
            )}
          </div>
        )}

        {result && (
          <Alert>
            <AlertDescription>
              已采集账号：{result.nickname || result.email || result.id}
            </AlertDescription>
          </Alert>
        )}

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
          {result && (
            <Button onClick={() => onOpenChange(false)}>完成</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 按档位打开授权页：国际版用应用内隔离窗口，国内版继续走系统浏览器（零改动）。 */
async function openAuthPage(url: string, variant: WbVariant): Promise<void> {
  if (variant === "ai") return api.openOauthWindow(url);
  return openInBrowser(url);
}

/** 收起应用内授权窗口；仅国际版会创建它，国内版无窗口可关，非桌面端为 no-op。 */
function closeAuthWindow(variant: WbVariant): void {
  if (variant !== "ai") return;
  void api.closeOauthWindow();
}

/** WebUI 使用浏览器新标签页，Tauri 使用系统 opener。 */
async function openInBrowser(url: string): Promise<void> {
  if (api.isWebui()) {
    // 浏览器环境没有 Tauri 注入的 invoke；window.open 被拦截时由弹窗中的
    // 原生链接作为兜底，因此这里不把拦截视为 OAuth 失败。
    try {
      window.open(url, "_blank", "noopener,noreferrer");
    } catch {
      // 忽略自动弹窗失败；弹窗中已展示的原生链接仍可点击。
    }
    return;
  }

  const { openUrl } = await import("@tauri-apps/plugin-opener");
  return openUrl(url);
}
