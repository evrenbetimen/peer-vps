import "@xterm/xterm/css/xterm.css";

import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import { useEffect, useRef } from "react";

import { commands, inTauri } from "../bridge/commands";
import type { Instance } from "../bridge/types";
import { newSuffix } from "../lib/tail";

/**
 * Web terminal for a guest.
 *
 * In the desktop app it is the guest's real serial console (boot log,
 * cloud-init, login prompt) and keystrokes go to the guest, so you can log in
 * with the user and password shown next to the instance. The browser preview
 * has no guest, so it runs a tiny local shell.
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

  // The padding sits on the wrapper: FitAddon sizes rows from the host's box, and padding
  // inside it pushed the last row out of view.
  return (
    <div className="h-72 w-full overflow-hidden rounded-lg border border-slate-800 bg-slate-950 p-2">
      <div ref={host} className="h-full w-full" />
    </div>
  );
}

/** Stream the serial console and send keystrokes to it; returns a stop function. */
function streamConsole(term: XTerm, instance: Instance): () => void {
  let shown = "";
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  // Poll quickly while someone is typing so the guest's echo shows up promptly.
  let fastUntil = 0;
  term.writeln(`\x1b[90mserial console of ${instance.id} · click here and type to log in\x1b[0m`);

  let sending = Promise.resolve();
  let warned = false;
  const input = term.onData((data) => {
    fastUntil = Date.now() + 3000;
    // Keep keystrokes in order; one failure (e.g. a guest on another machine) is reported once.
    sending = sending
      .then(() => commands.sendConsole(instance.id, data))
      .catch((e: unknown) => {
        if (warned) return;
        warned = true;
        term.writeln(`\r\n\x1b[33mcannot type here: ${e instanceof Error ? e.message : String(e)}\x1b[0m`);
      });
  });

  const tick = async () => {
    try {
      const text = await commands.getConsole(instance.id);
      if (stopped) return;
      if (text === null) {
        term.writeln("\x1b[33mThis node's hypervisor does not capture a console.\x1b[0m");
        return;
      }
      // The backend returns a bounded tail; append the new part, or redraw if it no longer overlaps.
      const added = newSuffix(shown, text);
      if (added !== null) term.write(added.replace(/\r?\n/g, "\r\n"));
      else {
        term.reset();
        term.write(text.replace(/\r?\n/g, "\r\n"));
      }
      shown = text;
    } catch {
      // Instance gone or scaled to zero; keep polling quietly.
    }
    if (stopped) return;
    clearTimeout(timer);
    timer = setTimeout(() => void tick(), Date.now() < fastUntil ? 150 : 1000);
  };
  void tick();
  return () => {
    stopped = true;
    clearTimeout(timer);
    input.dispose();
  };
}
