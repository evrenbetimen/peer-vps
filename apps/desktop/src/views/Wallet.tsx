import { useCallback, useEffect, useState } from "react";

import { commands } from "../bridge/commands";
import { useLive } from "../bridge/events";
import type { AccountSummary } from "../bridge/types";
import { Button, Card, ErrorNote, Stat } from "../components/ui";
import { credits, cx, time } from "../lib/format";

const LABELS: Record<string, string> = {
  top_up: "Top-up",
  usage: "Compute usage",
  platform_fee: "Platform fee",
  slash_compensation: "SLA compensation",
  collateral_lock: "Collateral locked",
  collateral_unlock: "Collateral released",
  pool_stake: "Pool stake",
};

export function Wallet() {
  const [summary, setSummary] = useState<AccountSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const live = useLive((s) => (summary ? s.balances[summary.account] : undefined));

  const refresh = useCallback(async () => {
    try {
      setSummary(await commands.getWallet());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Ledger rows are journaled server-side; refetch when the live balance moves.
  useEffect(() => {
    if (live !== undefined) void refresh();
  }, [live, refresh]);

  const topUp = async (amount: number) => {
    setError(null);
    try {
      await commands.topUp(amount);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (!summary) return <p className="text-slate-400">Loading wallet…</p>;
  const burn = summary.history.filter((h) => h.kind === "usage").slice(0, 20);

  return (
    <div className="space-y-4">
      <div className="grid gap-4 md:grid-cols-3">
        <Stat label="Available credits" value={`${credits(live ?? summary.balance)} cr`} hint={summary.account} />
        <Stat label="Recent usage" value={`${credits(-burn.reduce((s, h) => s + h.delta, 0))} cr`} hint={`last ${burn.length} settlements`} />
        <Card title="Top up">
          <div className="flex flex-wrap gap-2">
            {[5, 25, 100].map((c) => (
              <Button key={c} variant="ghost" onClick={() => void topUp(c * 1_000_000)}>
                +{c} cr
              </Button>
            ))}
          </div>
          <p className="mt-2 text-xs text-slate-500">Simulated gateway; runs through the signed-webhook verifier.</p>
        </Card>
      </div>
      <ErrorNote error={error} />
      <Card title="Billing log">
        <table className="w-full text-left text-sm">
          <thead className="text-xs uppercase text-slate-500">
            <tr>
              <th className="py-1">Time</th>
              <th>Entry</th>
              <th>Reference</th>
              <th className="text-right">Amount</th>
              <th className="text-right">Balance</th>
            </tr>
          </thead>
          <tbody className="font-mono">
            {summary.history.map((h) => (
              <tr key={h.id} className="border-t border-slate-800">
                <td className="py-1.5 text-slate-400">{time(h.at)}</td>
                <td className="font-sans">{LABELS[h.kind] ?? h.kind}</td>
                <td className="max-w-48 truncate text-slate-500">{h.reference ?? "—"}</td>
                <td className={cx("text-right", h.delta >= 0 ? "text-emerald-400" : "text-rose-300")}>
                  {h.delta >= 0 ? "+" : ""}
                  {credits(h.delta, 4)}
                </td>
                <td className="text-right">{credits(h.balanceAfter)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
    </div>
  );
}
