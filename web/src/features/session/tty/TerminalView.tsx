import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";

export function TerminalView({ instanceId }: { instanceId: string }) {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const term = new Terminal({
      fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
      fontSize: 14,
      theme: { background: "#12161C", foreground: "#E7DCC8", cursor: "#C9842A" },
    });
    const fit = new FitAddon();
    const unicode = new Unicode11Addon();
    term.loadAddon(fit);
    term.loadAddon(new SearchAddon());
    term.loadAddon(new WebLinksAddon());
    term.loadAddon(unicode);
    term.unicode.activeVersion = "11";
    term.open(el);
    fit.fit();
    term.writeln(`tty attach placeholder · instance ${instanceId}`);
    term.writeln("print driver has no TUI; this view is for claude-pty / claude-bg.");
    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
      term.dispose();
    };
  }, [instanceId]);

  return <div ref={host} style={{ height: "100%", minHeight: 240, padding: 8 }} />;
}
