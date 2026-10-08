import "@xterm/xterm/css/xterm.css";

import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import { useEffect, useRef } from "react";

import type { Instance } from "../bridge/types";

/**
 * Web terminal bound to a guest's virtual IP.
 *
 * The SSH transport (client ⇄ Noise tunnel ⇄ guest :22) is not wired yet, so
 * this runs a tiny local shell that explains where it will connect. The xterm
 * instance, sizing and input handling are the real ones.
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
    const ro = new ResizeObserver(() => fit.fit());
    ro.observe(host.current);
    return () => {
      ro.disconnect();
      sub.dispose();
      term.dispose();
    };
  }, [instance]);

  return <div ref={host} className="h-72 w-full overflow-hidden rounded-lg border border-slate-800 bg-slate-950 p-2" />;
}
