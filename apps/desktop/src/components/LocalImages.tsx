import { useCallback, useEffect, useState } from "react";

import { commands } from "../bridge/commands";
import type { Images, LocalImage } from "../bridge/types";
import { Button, Card, ErrorNote } from "./ui";

const gib = (bytes: number) => `${(bytes / 1024 ** 3).toFixed(1)} GiB`;

/** "Windows installer · ARM64" for an ISO, from its volume label. */
export function imageTitle(image: LocalImage | undefined): string {
  if (!image) return "Custom image";
  if (image.kind === "disk") return "Custom disk";
  const what = image.iso?.windows ? "Windows installer" : "Installer ISO";
  const arch = image.iso?.arch === "aarch64" ? " · ARM64" : image.iso?.arch === "x86_64" ? " · x64" : "";
  return `${what}${arch}`;
}

/** Guest OS images on this machine, and downloads from the catalog. */
export function LocalImages() {
  const [images, setImages] = useState<Images | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setImages(await commands.listImages());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  const downloading = images ? Object.values(images.downloads).some((d) => !d.error) : false;
  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), downloading ? 500 : 5000);
    return () => clearInterval(id);
  }, [refresh, downloading]);

  if (!images) return null;
  const names = [
    ...new Set([...images.catalog.map((c) => c.name), ...images.installed.map((i) => i.name), ...Object.keys(images.downloads)]),
  ];

  const importFile = async () => {
    setError(null);
    try {
      await commands.importImage();
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const pull = async (name: string) => {
    setError(null);
    try {
      await commands.pullImage(name);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <Card
      title="Guest images"
      actions={
        <Button variant="ghost" onClick={() => void importFile()}>
          Add ISO or disk…
        </Button>
      }
    >
      <ul className="divide-y divide-slate-800 text-sm">
        {names.map((name) => {
          const local = images.installed.find((i) => i.name === name);
          const title = images.catalog.find((c) => c.name === name)?.title ?? imageTitle(local);
          const dl = images.downloads[name];
          const pct = dl?.total ? Math.floor((dl.done / dl.total) * 100) : null;
          return (
            <li key={name} className="flex items-center justify-between gap-3 py-2" data-testid={`image-${name}`}>
              <div>
                <div className="font-mono">{name}</div>
                <div className="text-xs text-slate-400">{title}</div>
              </div>
              {local ? (
                <span className="text-xs text-emerald-400">installed · {gib(local.sizeBytes)}</span>
              ) : dl && !dl.error ? (
                <span className="font-mono text-xs text-cyan-300">
                  {dl.import ? "copying…" : pct === null ? "downloading…" : `downloading ${pct}%`}
                </span>
              ) : (
                <Button variant="ghost" onClick={() => void pull(name)}>
                  {dl?.error ? "Retry" : "Download"}
                </Button>
              )}
            </li>
          );
        })}
      </ul>
      {Object.entries(images.downloads)
        .filter(([, d]) => d.error)
        .map(([name, d]) => (
          <ErrorNote key={name} error={`${name}: ${d.error}`} />
        ))}
      <ErrorNote error={error} />
      <p className="mt-2 truncate text-xs text-slate-500" title={images.dir}>
        ISOs (a Windows installer too) and qcow2 disks in {images.dir} can be deployed from the Console.
      </p>
    </Card>
  );
}

/** Shown when no hypervisor was found, with the install command for this OS. */
export function InstallQemu({ reason }: { reason: string }) {
  const ua = navigator.userAgent;
  const cmd = /Mac/.test(ua)
    ? "brew install qemu"
    : /Windows/.test(ua)
      ? "winget install SoftwareFreedomConservancy.QEMU"
      : "sudo apt install qemu-system qemu-utils";
  return (
    <Card title="Real VMs are off">
      <p className="text-sm text-slate-300">Guests are simulated because QEMU was not found. Install it and restart PeerVPS:</p>
      <pre className="mt-2 rounded bg-slate-950 p-2 font-mono text-xs text-cyan-300">{cmd}</pre>
      <p className="mt-2 text-xs text-slate-500">{reason}</p>
    </Card>
  );
}
