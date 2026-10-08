import "@xterm/xterm/css/xterm.css";

import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import { useEffect, useRef } from "react";

import { commands, inTauri } from "../bridge/commands";
import type { Instance } from "../bridge/types";

/**
 * Web terminal for a guest.
 *
 * In the desktop app it streams the guest's real serial console (boot log,
 * cloud-init, login prompt); log in over the SSH command shown next to the
 * instance. The browser preview has no guest, so it runs a tiny local shell.
 */
export function Terminal({ instance }: { instance: Instance }) {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!host.current) return;
    const term = new XTerm({
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 13,
      cursorBlink: true,
      theme: { background: "#020617", foreground: "#e2e8f0", cursor: "#22d3ee" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();
    const ro = new ResizeObserver(() => fit.fit());
    ro.observe(host.current);

    if (inTauri) {
      const stop = streamConsole(term, instance);
      return () => {
        stop();
        ro.disconnect();
        term.dispose();
      };
    }

    const prompt = () => term.write(`\r\n\x1b[36mubuntu@${instance.virtualIp}\x1b[0m:~$ `);
    term.writeln(`\x1b[90mpeervps ssh ${instance.id} → ${instance.virtualIp}:22 (${instance.spec.image})\x1b[0m`);
    term.writeln("\x1b[33mOverlay SSH transport not connected yet; local echo shell for now.\x1b[0m");
    prompt();

    let line = "";
    const sub = term.onData((data) => {
      for (const ch of data) {
        if (ch === "\r") {
          const cmd = line.trim();
          line = "";
          if (cmd === "clear") term.clear();
          else if (cmd === "help") term.write("\r\ncommands: help, whoami, hostname, ip, clear");
          else if (cmd === "whoami") term.write("\r\nubuntu");
          else if (cmd === "hostname") term.write(`\r\n${instance.id}`);
          else if (cmd === "ip") term.write(`\r\npvps0: ${instance.virtualIp}/16 (overlay)`);
          else if (cmd) term.write(`\r\n${cmd}: command not available in the local shell`);
          prompt();
        } else if (ch === "\u007f") {
          if (line) {
            line = line.slice(0, -1);
            term.write("\b \b");
          }
        } else if (ch >= " ") {
          line += ch;
          term.write(ch);
        }
      }
    });
    return () => {
      ro.disconnect();
      sub.dispose();
      term.dispose();
    };
  }, [instance]);

  return <div ref={host} className="h-72 w-full overflow-hidden rounded-lg border border-slate-800 bg-slate-950 p-2" />;
}

/** Poll the serial console and append what is new; returns a stop function. */
function streamConsole(term: XTerm, instance: Instance): () => void {
  let shown = "";
  let stopped = false;
  term.writeln(`\x1b[90mserial console of ${instance.id} (read-only)\x1b[0m`);
  const tick = async () => {
    try {
      const text = await commands.getConsole(instance.id);
      if (stopped) return;
      if (text === null) {
        term.writeln("\x1b[33mThis node's hypervisor does not capture a console.\x1b[0m");
        return;
      }
      // The backend returns a bounded tail; append the new suffix, or redraw if the window moved.
      if (text.startsWith(shown)) term.write(text.slice(shown.length).replace(/\r?\n/g, "\r\n"));
      else {
        term.clear();
        term.write(text.replace(/\r?\n/g, "\r\n"));
      }
      shown = text;
    } catch {
      // Instance gone or scaled to zero; keep polling quietly.
    }
    if (!stopped) setTimeout(() => void tick(), 1000);
  };
  void tick();
  return () => {
    stopped = true;
  };
}
