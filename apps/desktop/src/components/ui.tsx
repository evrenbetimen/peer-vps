import type { ReactNode } from "react";

import { cx } from "../lib/format";

export function Card({ title, actions, className, children }: { title?: ReactNode; actions?: ReactNode; className?: string; children: ReactNode }) {
  return (
    <section className={cx("rounded-xl border border-slate-800 bg-slate-900/60 p-4", className)}>
      {(title || actions) && (
        <header className="mb-3 flex items-center justify-between gap-2">
          <h2 className="text-xs font-semibold uppercase tracking-wider text-slate-400">{title}</h2>
          {actions}
        </header>
      )}
      {children}
    </section>
  );
}

export function Stat({ label, value, hint, tone = "default" }: { label: string; value: ReactNode; hint?: ReactNode; tone?: "default" | "good" | "warn" | "bad" }) {
  const toneClass = { default: "text-slate-100", good: "text-emerald-400", warn: "text-amber-400", bad: "text-rose-400" }[tone];
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
      <div className="text-xs uppercase tracking-wider text-slate-400">{label}</div>
      <div className={cx("mt-1 font-mono text-2xl tabular-nums", toneClass)}>{value}</div>
      {hint && <div className="mt-1 text-xs text-slate-500">{hint}</div>}
    </div>
  );
}

export function Button({
  children,
  onClick,
  variant = "primary",
  disabled,
  type = "button",
}: {
  children: ReactNode;
  onClick?: () => void;
  variant?: "primary" | "ghost" | "danger";
  disabled?: boolean;
  type?: "button" | "submit";
}) {
  const v = {
    primary: "bg-cyan-500 text-slate-950 hover:bg-cyan-400",
    ghost: "border border-slate-700 text-slate-200 hover:bg-slate-800",
    danger: "bg-rose-500/90 text-white hover:bg-rose-500",
  }[variant];
  return (
    <button type={type} onClick={onClick} disabled={disabled} className={cx("whitespace-nowrap rounded-lg px-3 py-1.5 text-sm font-medium transition disabled:cursor-not-allowed disabled:opacity-40", v)}>
      {children}
    </button>
  );
}

export function Slider({
  label,
  value,
  min,
  max,
  step = 1,
  format,
  onChange,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  step?: number;
  format: (v: number) => string;
  onChange: (v: number) => void;
}) {
  return (
    <label className="block">
      <div className="mb-1 flex justify-between text-sm">
        <span className="text-slate-300">{label}</span>
        <span className="font-mono tabular-nums text-cyan-300">{format(value)}</span>
      </div>
      <input
        type="range"
        className="w-full accent-cyan-400"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
      />
    </label>
  );
}

export function Toggle({ label, checked, onChange }: { label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="flex cursor-pointer items-center justify-between text-sm">
      <span className="text-slate-300">{label}</span>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={cx("relative h-6 w-11 rounded-full transition", checked ? "bg-cyan-500" : "bg-slate-700")}
      >
        <span className={cx("absolute top-0.5 h-5 w-5 rounded-full bg-white transition", checked ? "left-5" : "left-0.5")} />
      </button>
    </label>
  );
}

/** Minimal SVG sparkline; values are drawn against [0, max]. */
export function Sparkline({ values, max, className }: { values: number[]; max?: number; className?: string }) {
  const w = 240;
  const h = 48;
  if (values.length < 2) return <svg viewBox={`0 0 ${w} ${h}`} className={className} />;
  const top = max ?? Math.max(...values, 1);
  const pts = values.map((v, i) => `${(i / (values.length - 1)) * w},${h - (Math.min(v, top) / top) * (h - 2) - 1}`).join(" ");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" className={className}>
      <polyline points={`0,${h} ${pts} ${w},${h}`} className="fill-cyan-400/10" />
      <polyline points={pts} fill="none" className="stroke-cyan-400" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

export function ErrorNote({ error }: { error: string | null }) {
  if (!error) return null;
  return <p className="mt-2 rounded-lg border border-rose-900 bg-rose-950/50 px-3 py-2 text-sm text-rose-300">{error}</p>;
}
