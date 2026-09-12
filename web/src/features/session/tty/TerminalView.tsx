import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { useWorkbenchViewport } from "../../../lib/viewport";
import type { Instance } from "../../../types/instance";
import { payloadForStreamWrite, stripAnsi } from "./applyFrame";
import { AuxKeys } from "./AuxKeys";
import { openTtySession, type TtySession, type TtyStatus } from "./client";
import { LocalInput } from "./LocalInput";
import { createFontMeasure, fittedTerminalFont, responsiveTerminalSize, whenFontsReady } from "./terminalFit";
import { attachTerminalTouch } from "./terminalTouch";
import { NIGHT_CORRAL_THEME, TERMINAL_FONT_FAMILY } from "./theme";
import css from "./TerminalView.module.css";

export type TtyLabHandle = {
  disconnect: () => void;
  reconnect: () => void;
};

declare global {
  interface Window {
    __ttyLab?: TtyLabHandle;
  }
}

type DisplayMode = "fit" | "fixed" | "responsive";

export function TerminalView({
  instance,
  onAttachFailed,
}: {
  instance: Instance;
  onAttachFailed?: (reason: string) => void;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewportRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const searchRef = useRef<SearchAddon | null>(null);
  const sessionRef = useRef<TtySession | null>(null);
  const resetStreamRef = useRef(true);
  const generationRef = useRef(0);
  const { mobile, offsetTop } = useWorkbenchViewport();
  const [inputOverride, setInputOverride] = useState<{ direct: boolean; mode: DisplayMode } | null>(null);
  const [status, setStatus] = useState<TtyStatus>("connecting");
  const [cols, setCols] = useState(80);
  const [rows, setRows] = useState(24);
  const directInput = inputOverride?.direct ?? !mobile;
  const mode = inputOverride?.mode ?? (mobile ? "responsive" : "fit");
  const modeRef = useRef<DisplayMode>(mode);
  const failRef = useRef(onAttachFailed);
  const applyFitRef = useRef<() => void>(() => {});
  const instanceRef = useRef(instance);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [preview, setPreview] = useState("");
  const [ready, setReady] = useState(false);
  const frozen = status === "reconnecting" || status === "failed";

  const send = (data: string) => {
    void sessionRef.current?.write(data);
  };

  useEffect(() => {
    failRef.current = onAttachFailed;
  }, [onAttachFailed]);
  useEffect(() => {
    modeRef.current = mode;
  }, [mode]);
  useEffect(() => {
    instanceRef.current = instance;
  }, [instance]);

  useEffect(() => {
    const host = hostRef.current;
    const viewport = viewportRef.current;
    if (!host || !viewport) return;

    const term = new Terminal({
      allowProposedApi: true,
      cursorBlink: true,
      cursorStyle: "block",
      fontFamily: TERMINAL_FONT_FAMILY,
      fontSize: 14,
      lineHeight: 1,
      scrollback: 0,
      convertEol: false,
      disableStdin: true,
      screenReaderMode: true,
      theme: NIGHT_CORRAL_THEME,
    });
    const fit = new FitAddon();
    const search = new SearchAddon();
    const unicode = new Unicode11Addon();
    term.loadAddon(fit);
    term.loadAddon(search);
    term.loadAddon(new WebLinksAddon());
    term.loadAddon(unicode);
    term.unicode.activeVersion = "11";
    term.open(host);
    termRef.current = term;
    searchRef.current = search;
    const font = createFontMeasure(host, TERMINAL_FONT_FAMILY);

    const applyFit = () => {
      if (!termRef.current || !viewportRef.current) return;
      const bounds = {
        width: viewport.clientWidth,
        height: viewport.clientHeight,
        dpr: window.devicePixelRatio || 1,
        lineHeight: term.options.lineHeight || 1,
        letterSpacing: term.options.letterSpacing || 0,
      };
      if (modeRef.current === "responsive") {
        const size = responsiveTerminalSize(bounds, font.measure, 14);
        if (size && (term.cols !== size.cols || term.rows !== size.rows)) term.resize(size.cols, size.rows);
        if (size) {
          setCols(size.cols);
          setRows(size.rows);
          void sessionRef.current?.resize(size.cols, size.rows);
        }
        return;
      }
      fit.fit();
      if (modeRef.current === "fit") {
        const fitted = fittedTerminalFont(
          { ...bounds, cols: term.cols, rows: term.rows },
          font.measure,
          14,
        );
        if (fitted != null && term.options.fontSize !== fitted) term.options.fontSize = fitted;
        fit.fit();
      }
      setCols(term.cols);
      setRows(term.rows);
      void sessionRef.current?.resize(term.cols, term.rows);
    };

    const inputDisposable = term.onData((data) => {
      if (term.options.disableStdin) return;
      send(data);
    });

    applyFitRef.current = applyFit;
    void whenFontsReady().then(() => applyFitRef.current());
    applyFit();

    const session = openTtySession(instanceRef.current, {
      onFrame: (payload, _offset, _streamId, representation) => {
        const reset = resetStreamRef.current;
        if (reset && representation === "rendered-ansi") term.options.scrollback = 0;
        const bytes = payloadForStreamWrite(payload, reset && representation === "rendered-ansi");
        resetStreamRef.current = false;
        const text = stripAnsi(payload);
        setPreview((current) => (reset ? text : current + text));
        term.write(bytes, () => {
          setReady(true);
        });
      },
      onStatus: (next, message) => {
        setStatus(next);
        if (next === "connecting") resetStreamRef.current = true;
        if (next === "failed") failRef.current?.(message ?? "tty.attach failed");
      },
    });
    sessionRef.current = session;
    generationRef.current += 1;

    const observer = new ResizeObserver(() => applyFit());
    observer.observe(viewport);
    const onViewport = () => {
      if (window.visualViewport && window.visualViewport.scale !== 1) return;
      applyFit();
    };
    window.visualViewport?.addEventListener("resize", onViewport);
    window.addEventListener("resize", onViewport);

    const detachTouch = attachTerminalTouch(viewport, {
      onScrollPixels: (deltaY) => {
        const line = Math.max(1, host.querySelector<HTMLElement>(".xterm-rows > div")?.getBoundingClientRect().height || 16);
        term.scrollLines(Math.trunc(deltaY / line));
      },
      getGeneration: () => generationRef.current,
      hasSelection: () => term.hasSelection(),
    });

    window.__ttyLab = {
      disconnect: () => session.disconnectForTest(),
      reconnect: () => {
        resetStreamRef.current = true;
        session.reconnectForTest();
      },
    };

    return () => {
      delete window.__ttyLab;
      detachTouch();
      observer.disconnect();
      window.visualViewport?.removeEventListener("resize", onViewport);
      window.removeEventListener("resize", onViewport);
      inputDisposable.dispose();
      font.dispose();
      void session.detach();
      sessionRef.current = null;
      term.dispose();
      termRef.current = null;
    };
  }, [instance.id]);

  useEffect(() => {
    applyFitRef.current();
  }, [mode]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.disableStdin = frozen || !directInput;
    if (directInput && !frozen) term.focus();
  }, [directInput, frozen]);

  return (
    <section
      className={css.lab}
      data-tty-lab="1"
      data-tty-ready={ready ? "1" : "0"}
      data-tty-status={status}
      data-tty-cols={cols}
      data-tty-rows={rows}
      style={{ paddingBottom: offsetTop ? 0 : undefined }}
    >
      <header className={css.toolbar}>
        <span className={css.geo}>
          {cols}×{rows} · {mode} · Unicode11 · WebLinks
        </span>
        <div className={css.seg}>
          <button
            type="button"
            className={!directInput ? css.segOn : undefined}
            aria-pressed={!directInput}
            onClick={() => setInputOverride((current) => ({ direct: false, mode: current?.mode ?? mode }))}
          >
            本地输入
          </button>
          <button
            type="button"
            className={directInput ? css.segOn : undefined}
            aria-pressed={directInput}
            onClick={() => setInputOverride((current) => ({ direct: true, mode: current?.mode ?? mode }))}
          >
            直连
          </button>
        </div>
        {!mobile ? <AuxKeys disabled={frozen} onKey={send} variant="toolbar" /> : null}
        {!mobile ? (
          <button
            type="button"
            className={css.geo}
            onClick={() =>
              setInputOverride((current) => {
                const next = mode === "fit" ? "fixed" : mode === "fixed" ? "responsive" : "fit";
                return { direct: current?.direct ?? directInput, mode: next };
              })
            }
          >
            {mode}
          </button>
        ) : null}
        {!mobile ? (
          <button type="button" className={css.geo} aria-pressed={searchOpen} onClick={() => setSearchOpen((open) => !open)}>
            搜索
          </button>
        ) : null}
        {searchOpen && !mobile ? (
          <form
            className={css.search}
            onSubmit={(event) => {
              event.preventDefault();
              if (searchQuery) searchRef.current?.findNext(searchQuery);
            }}
          >
            <input aria-label="搜索终端" value={searchQuery} onChange={(e) => setSearchQuery(e.target.value)} />
            <button
              type="button"
              className={css.geo}
              onClick={() => {
                if (searchQuery) searchRef.current?.findNext(searchQuery);
              }}
            >
              下一个
            </button>
          </form>
        ) : null}
      </header>
      {status !== "live" ? (
        <div className={css.banner} role="status">
          <span className={css.dots} aria-hidden>
            <span className={css.dot} />
            <span className={css.dot} />
            <span className={css.dot} />
          </span>
          {status === "connecting"
            ? "正在连接终端…"
            : status === "reconnecting"
              ? "reconnecting · 终端保留最后一帧，不清屏"
              : "终端连接失败"}
          {status === "reconnecting" || status === "failed" ? (
            <button
              type="button"
              className={css.geo}
              onClick={() => {
                resetStreamRef.current = true;
                sessionRef.current?.reconnectForTest();
              }}
            >
              重连
            </button>
          ) : null}
        </div>
      ) : null}
      <div className={css.viewport} ref={viewportRef} role="region" aria-label="终端画面">
        <div className={css.host} ref={hostRef} />
        <pre className={css.preview} data-testid="tty-ansi-preview">
          {preview}
        </pre>
      </div>
      <div className={css.dock}>
        {directInput ? (
          <>
            <span className={css.dockLabel}>本地输入</span>
            <div className={css.directGhost}>直连开启中 —— 击键直接进 PTY</div>
            <div className={css.directGhostSend}>发送</div>
          </>
        ) : (
          <LocalInput disabled={frozen} mobile={mobile} onSend={send} />
        )}
      </div>
      {mobile ? <AuxKeys disabled={frozen} onKey={send} /> : null}
      {!mobile ? (
        <div className={css.note}>TTY 字节走独立 raw_tty 流，不进 transcript 节点 · 结构化卡片仍在「结构」tab 看同一 journal</div>
      ) : null}
    </section>
  );
}
