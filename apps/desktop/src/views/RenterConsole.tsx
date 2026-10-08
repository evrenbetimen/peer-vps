import { lazy, Suspense, useCallback, useEffect, useState } from "react";

import { BridgeError, commands } from "../bridge/commands";
import { useLive } from "../bridge/events";
import type { AcceleratorKind, GuestAccess, Instance, LocalImage, Offer, OfferQuery, VmSpec } from "../bridge/types";
import { imageTitle } from "../components/LocalImages";
import { Button, Card, ErrorNote } from "../components/ui";
import { credits, cx, mib, perHour } from "../lib/format";

interface Template {
  image: string;
  name: string;
  blurb: string;
  accelerator: AcceleratorKind;
  /** Installed from an ISO: the installer runs on a screen the renter opens. */
  iso?: boolean;
  windows?: boolean;
}

/** The provider's own images (ISOs, custom disks) as templates. */
function localTemplates(installed: LocalImage[]): Template[] {
  return installed
    .filter((i) => !TEMPLATES.some((t) => t.image === i.name))
    .map((i) => ({
      image: i.name,
      name: i.name,
      blurb: imageTitle(i),
      accelerator: "none",
      iso: i.kind === "iso",
      windows: i.iso?.windows ?? false,
    }));
}

// xterm is the heaviest dependency; load it only when a shell is opened.
const Terminal = lazy(() => import("../components/Terminal").then((m) => ({ default: m.Terminal })));

const TEMPLATES: Template[] = [
  { image: "ubuntu-24.04", name: "Ubuntu 24.04", blurb: "Minimal server", accelerator: "none" },
  { image: "ubuntu-22.04", name: "Ubuntu 22.04", blurb: "Previous LTS", accelerator: "none" },
  { image: "debian-13", name: "Debian 13", blurb: "Stable base", accelerator: "none" },
  { image: "ubuntu-24.04-cuda", name: "Ubuntu + CUDA", blurb: "Drivers + toolkit", accelerator: "gpu" },
  { image: "vllm-llama", name: "vLLM inference", blurb: "OpenAI-compatible LLM server", accelerator: "gpu" },
  { image: "npu-runtime", name: "NPU runtime", blurb: "ONNX on edge NPUs", accelerator: "npu" },
];

const RAM = [1024, 2048, 4096, 8192, 16384, 32768];
const DISK = [10, 20, 40, 80, 160];

export function RenterConsole() {
  const [template, setTemplate] = useState<Template>(TEMPLATES[0]!);
  const [local, setLocal] = useState<Template[]>([]);
  const [spec, setSpec] = useState({ vcpus: 2, memMib: 4096, diskGib: 20 });
  const [minVramGib, setMinVramGib] = useState(0);
  const [maxPerHour, setMaxPerHour] = useState(100);
  const [offers, setOffers] = useState<Offer[]>([]);
  const [selectedOffer, setSelectedOffer] = useState<string | null>(null);
  const [instances, setInstances] = useState<Instance[]>([]);
  const [shell, setShell] = useState<Instance | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const liveBalance = useLive((s) => s.balances["demo-agent"]);
  const [initialBalance, setInitialBalance] = useState<number | undefined>();
  const balance = liveBalance ?? initialBalance;

  const loadOffers = useCallback(async () => {
    const q: OfferQuery = { sort: "price", maxPricePerHour: maxPerHour * 1_000_000, minVcpus: spec.vcpus, minMemMib: spec.memMib };
    if (minVramGib > 0) q.minVramMib = minVramGib * 1024;
    if (template.accelerator !== "none") q.accelerator = template.accelerator;
    const list = await commands.listOffers(q);
    setOffers(list);
    setSelectedOffer((cur) => (cur && list.some((o) => o.id === cur) ? cur : (list[0]?.id ?? null)));
  }, [maxPerHour, minVramGib, spec.vcpus, spec.memMib, template.accelerator]);

  const loadInstances = useCallback(async () => {
    const [list, wallet] = await Promise.all([commands.listInstances(), commands.getWallet()]);
    setInstances(list);
    setInitialBalance(wallet.balance);
  }, []);

  useEffect(() => {
    void loadOffers().catch((e) => setError(String(e)));
  }, [loadOffers]);
  useEffect(() => {
    void loadInstances();
  }, [loadInstances]);
  useEffect(() => {
    const load = () => commands.listImages().then((i) => setLocal(localTemplates(i.installed)), () => setLocal([]));
    void load();
    const id = setInterval(() => void load(), 5000);
    return () => clearInterval(id);
  }, []);

  const pick = (t: Template) => {
    setTemplate(t);
    // Windows 11 needs 4 GiB of RAM and Setup alone fills ~20 GiB.
    if (t.windows) setSpec((s) => ({ ...s, memMib: Math.max(s.memMib, 4096), diskGib: Math.max(s.diskGib, 80) }));
  };

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
      await loadInstances();
    } catch (e) {
      setError(e instanceof BridgeError ? `${e.code}: ${e.message}` : String(e));
    } finally {
      setBusy(false);
    }
  };

  const deploy = () =>
    run(async () => {
      if (!selectedOffer) throw new Error("pick an offer");
      const vmSpec: VmSpec = { ...spec, image: template.image, confidential: false };
      if (template.accelerator !== "none") vmSpec.accelerator = { kind: template.accelerator, slices: 1 };
      const inst = await commands.deployInstance(selectedOffer, vmSpec);
      setShell(inst);
    });

  return (
    <div className="space-y-4">
      <div className="grid gap-4 xl:grid-cols-[1fr_380px]">
        <Card title="1 · Template">
          <div className="grid grid-cols-2 gap-2 md:grid-cols-3">
            {[...TEMPLATES, ...local].map((t) => (
              <button
                key={t.image}
                type="button"
                aria-pressed={t.image === template.image}
                onClick={() => pick(t)}
                className={cx(
                  "rounded-lg border p-3 text-left transition",
                  t.image === template.image ? "border-cyan-400 bg-cyan-400/10" : "border-slate-800 hover:border-slate-600",
                )}
              >
                <div className="text-sm font-medium">{t.name}</div>
                <div className="text-xs text-slate-400">{t.blurb}</div>
                {t.accelerator !== "none" && <div className="mt-1 text-[10px] uppercase tracking-wider text-violet-300">{t.accelerator}</div>}
                {t.iso && <div className="mt-1 text-[10px] uppercase tracking-wider text-amber-300">installer</div>}
              </button>
            ))}
          </div>
          {template.iso && (
            <p className="mt-3 text-xs text-slate-400">
              Boots the installer with a blank disk. Open its screen from the instance list to finish setup
              {template.windows ? "; Windows signs in the user shown there and turns on Remote Desktop." : "."}
            </p>
          )}
        </Card>

        <Card title="2 · Size">
          <div className="space-y-3 text-sm">
            <Picker label="vCPU" options={[1, 2, 4, 8, 16]} value={spec.vcpus} fmt={String} onChange={(vcpus) => setSpec({ ...spec, vcpus })} />
            <Picker label="RAM" options={RAM} value={spec.memMib} fmt={mib} onChange={(memMib) => setSpec({ ...spec, memMib })} />
            <Picker label="Disk" options={DISK} value={spec.diskGib} fmt={(v) => `${v} GiB`} onChange={(diskGib) => setSpec({ ...spec, diskGib })} />
            <Picker label="Min VRAM" options={[0, 8, 12, 24, 40]} value={minVramGib} fmt={(v) => (v ? `${v} GiB` : "any")} onChange={setMinVramGib} />
            <Picker label="Max price" options={[5, 25, 100, 500]} value={maxPerHour} fmt={(v) => `${v} cr/h`} onChange={setMaxPerHour} />
          </div>
        </Card>
      </div>

      <Card
        title={`3 · Offers · ${offers.length}`}
        actions={
          <div className="flex items-center gap-3 text-sm">
            <span className="text-slate-400">Wallet {balance !== undefined ? `${credits(balance)} cr` : "…"}</span>
            <Button onClick={() => void deploy()} disabled={busy || !selectedOffer}>
              Deploy
            </Button>
          </div>
        }
      >
        <table className="w-full text-left text-sm">
          <thead className="text-xs uppercase text-slate-500">
            <tr>
              <th className="py-1" />
              <th>Offer</th>
              <th>Region</th>
              <th>Compute</th>
              <th>Accelerator</th>
              <th>SLA</th>
              <th className="text-right">Price</th>
            </tr>
          </thead>
          <tbody>
            {offers.map((o) => (
              <tr key={o.id} onClick={() => setSelectedOffer(o.id)} className={cx("cursor-pointer border-t border-slate-800", o.id === selectedOffer && "bg-cyan-400/5")}>
                <td className="py-1.5">
                  <input type="radio" readOnly checked={o.id === selectedOffer} className="accent-cyan-400" />
                </td>
                <td className="font-mono">
                  {o.id}
                  {o.confidential && <span className="ml-2 rounded bg-emerald-500/15 px-1 text-[10px] text-emerald-300">TEE</span>}
                </td>
                <td>{o.region}</td>
                <td>
                  {o.vcpus} vCPU · {mib(o.memMib)}
                </td>
                <td>{o.acceleratorModel ? `${o.acceleratorModel} · ${mib(o.vramMib)}` : "—"}</td>
                <td>{o.slaPct.toFixed(2)}%</td>
                <td className="text-right font-mono">{perHour(o.pricePerSec)}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {offers.length === 0 && <p className="py-3 text-sm text-slate-500">No offer matches. Loosen the size or price filters.</p>}
        <ErrorNote error={error} />
      </Card>

      <div className="grid gap-4 xl:grid-cols-2">
        <Card title={`Instances · ${instances.filter((i) => i.state !== "terminated").length} active`}>
          <ul className="divide-y divide-slate-800 text-sm">
            {instances.map((i) => (
              <li key={i.id} className="flex items-center justify-between gap-2 py-2">
                <div>
                  <div className="font-mono">{i.id}</div>
                  <div className="text-xs text-slate-400">
                    {i.spec.image} · {i.virtualIp} · {perHour(i.pricePerSec)} · <span className={cx(i.state === "running" ? "text-emerald-400" : "text-slate-500")}>{i.state}</span>
                  </div>
                  {i.state !== "terminated" && <AccessLine instance={i} />}
                </div>
                {i.state !== "terminated" && (
                  <div className="flex gap-1.5">
                    <Button variant="ghost" onClick={() => setShell(i)} disabled={i.state !== "running"}>
                      Shell
                    </Button>
                    <Button variant="ghost" disabled={busy} onClick={() => void run(() => commands.scaleInstance(i.id, i.state === "running" ? 0 : 1))}>
                      {i.state === "running" ? "Scale to 0" : "Resume"}
                    </Button>
                    <Button variant="danger" disabled={busy} onClick={() => void run(() => commands.terminateInstance(i.id))}>
                      Terminate
                    </Button>
                  </div>
                )}
              </li>
            ))}
            {instances.length === 0 && <li className="py-2 text-slate-500">Nothing deployed yet.</li>}
          </ul>
        </Card>
        <Card title={shell ? `SSH · ${shell.virtualIp}` : "SSH"}>
          {shell ? (
            <Suspense fallback={<p className="text-sm text-slate-500">Loading terminal…</p>}>
              <Terminal instance={shell} />
            </Suspense>
          ) : <p className="text-sm text-slate-500">Open a shell on a running instance.</p>}
        </Card>
      </div>
    </div>
  );
}

function Picker<T extends number>({ label, options, value, fmt, onChange }: { label: string; options: T[]; value: T; fmt: (v: T) => string; onChange: (v: T) => void }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <span className="w-20 text-slate-400">{label}</span>
      <div role="group" aria-label={label} className="flex flex-1 flex-wrap justify-end gap-1">
        {options.map((o) => (
          <button
            key={o}
            type="button"
            aria-pressed={o === value}
            onClick={() => onChange(o)}
            className={cx("rounded-md px-2 py-1 font-mono text-xs", o === value ? "bg-cyan-500 text-slate-950" : "bg-slate-800 text-slate-300 hover:bg-slate-700")}
          >
            {fmt(o)}
          </button>
        ))}
      </div>
    </div>
  );
}

/** How to reach a guest: SSH for cloud images; screen and Remote Desktop for ISO installs. */
function AccessLine({ instance }: { instance: Instance }) {
  const [access, setAccess] = useState<GuestAccess | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    commands.getInstanceAccess(instance.id).then(setAccess, () => setAccess(null));
  }, [instance.id, instance.state]);
  if (!access) return null;
  const cmd = access.windows
    ? `${access.rdp ?? ""}`
    : `ssh -p ${access.sshPort} ${access.user ? `${access.user}@` : ""}${access.sshHost}`;
  const copy = () => {
    void navigator.clipboard?.writeText(cmd).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    });
  };
  const openScreen = () => {
    setError(null);
    commands.openGuestScreen(instance.id).catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  };
  return (
    <div className="mt-1 flex flex-wrap items-center gap-x-2 text-xs">
      {access.windows && <span className="text-slate-400">Remote Desktop</span>}
      <button type="button" onClick={copy} className="font-mono text-cyan-300 hover:underline" title="Copy">
        {cmd}
      </button>
      {copied && <span className="text-emerald-400">copied</span>}
      {access.windows && access.user && (
        <span className="text-slate-400">
          user <span className="font-mono text-slate-200">{access.user}</span>
        </span>
      )}
      {access.password && (
        <span className="text-slate-400">
          password <span className="font-mono text-slate-200">{access.password}</span>
        </span>
      )}
      {!access.user && <span className="text-slate-400">login is set during installation</span>}
      {access.display && (
        <>
          <button type="button" onClick={openScreen} className="text-amber-300 hover:underline" title={access.display}>
            Open screen
          </button>
          {access.displayPassword && (
            <span className="text-slate-400">
              screen password <span className="font-mono text-slate-200">{access.displayPassword}</span>
            </span>
          )}
        </>
      )}
      {error && <span className="text-rose-400">{error}</span>}
    </div>
  );
}
