// ─── LiveBanner — celebration block ─────────────────────────────
//
// Ink-black surface with a tomato circle in the corner. Shown at
// the top of the workspace right after a successful deploy.
// Surfaces the URL pill (copyable) and four follow-on actions.

import { useState } from "react";

export interface LiveBannerProps {
  appName: string;
  appUrl: string;
  shippedAgo: string;
  onCustomDomain?: () => void;
  onShare?: () => void;
  onAddPrice?: () => void;
  onDismiss?: () => void;
}

export function LiveBanner({ appName, appUrl, shippedAgo, onCustomDomain, onShare, onAddPrice, onDismiss }: LiveBannerProps) {
  const [copied, setCopied] = useState(false);

  function copy() {
    navigator.clipboard?.writeText(appUrl);
    setCopied(true);
    setTimeout(() => setCopied(false), 1400);
  }

  return (
    <div
      className="relative bg-ink text-paper px-10 py-9 overflow-hidden"
      data-testid="live-banner"
    >
      <span
        className="pointer-events-none absolute -top-5 -right-5 size-[200px] rounded-full"
        style={{ border: "22px solid var(--color-tomato)", opacity: 0.10 }}
        aria-hidden="true"
      />
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label="Dismiss"
          className="absolute right-3 top-3 size-6 inline-flex items-center justify-center text-paper/60 hover:text-paper bg-transparent border-0 cursor-pointer"
        >
          ✕
        </button>
      )}
      <div className="font-sans text-[10.5px] uppercase tracking-[0.22em] text-tomato mb-3">
        Just shipped, {shippedAgo}
      </div>
      <h2 className="font-serif text-[38px] leading-[1.04] -tracking-[0.015em] mb-4 font-normal">
        {appName} is <em className="italic text-tomato">live.</em>
      </h2>
      <div className="inline-flex items-center gap-2 px-3 py-2 bg-paper text-ink rounded-full font-mono text-[12.5px] font-medium" style={{ boxShadow: "0 0 0 1px var(--color-tomato)" }}>
        <a href={appUrl.startsWith("http") ? appUrl : `https://${appUrl}`} target="_blank" rel="noreferrer" className="text-ink hover:opacity-80" style={{ textDecoration: "none" }}>
          {appUrl.replace(/^https?:\/\//, "")}
        </a>
        <button
          type="button"
          onClick={copy}
          className="font-sans text-[9.5px] uppercase tracking-[0.18em] text-ink-soft border-l border-rule pl-2 hover:text-ink bg-transparent border-0 cursor-pointer"
          style={{ borderLeft: "1px solid var(--color-rule)" }}
        >
          {copied ? "copied ✓" : "copy"}
        </button>
      </div>
      <div className="mt-5 flex flex-wrap gap-x-5 gap-y-2 text-paper">
        <a href={appUrl.startsWith("http") ? appUrl : `https://${appUrl}`} target="_blank" rel="noreferrer" className="font-sans text-[11px] uppercase tracking-[0.18em] hover:opacity-80" style={{ borderBottom: "1px solid var(--color-tomato)", paddingBottom: "2px", textDecoration: "none", color: "var(--color-paper)" }}>
          Open in tab ↗
        </a>
        {onCustomDomain && (
          <button onClick={onCustomDomain} className="font-sans text-[11px] uppercase tracking-[0.18em] hover:opacity-80 bg-transparent border-0 cursor-pointer" style={{ borderBottom: "1px solid var(--color-tomato)", paddingBottom: "2px", color: "var(--color-paper)" }}>
            Add a custom domain
          </button>
        )}
        {onShare && (
          <button onClick={onShare} className="font-sans text-[11px] uppercase tracking-[0.18em] hover:opacity-80 bg-transparent border-0 cursor-pointer" style={{ borderBottom: "1px solid var(--color-tomato)", paddingBottom: "2px", color: "var(--color-paper)" }}>
            Share
          </button>
        )}
        {onAddPrice && (
          <button onClick={onAddPrice} className="font-sans text-[11px] uppercase tracking-[0.18em] hover:opacity-80 bg-transparent border-0 cursor-pointer" style={{ borderBottom: "1px solid var(--color-tomato)", paddingBottom: "2px", color: "var(--color-paper)" }}>
            Add a price
          </button>
        )}
      </div>
    </div>
  );
}
