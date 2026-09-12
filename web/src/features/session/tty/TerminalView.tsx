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
import { binaryStringToBytes } from "./ids";
import { LocalInput } from "./LocalInput";
import { attachTerminalRenderer, type TerminalRenderer } from "./renderer";
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

function concatQueued(chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((n, chunk) => n + chunk.byteLength, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

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
  const directRef = useRef(true);
  const { mobile, offsetTop } = useWorkbenchViewport();
  const [inputOverride, setInputOverride] = useState<{ direct: boolean; mode: DisplayMode } | null>(null);
  const [status, setStatus] = useState<TtyStatus>("connecting");
  const [cols, setCols] = useState(80);
  const [rows, setRows] = useState(24);
  const [fullscreen, setFullscreen] = useState(false);
  const [renderer, setRenderer] = useState<TerminalRenderer>("dom");
  const [mouseMode, setMouseMode] = useState("none");
  const directInput = inputOverride?.direct ?? !mobile;
  const mode = inputOverride?.mode ?? (mobile ? "responsive" : "fit");
  const ioMode = directInput ? "raw" : "keys";
  const modeRef = useRef<DisplayMode>(mode);
  const failRef = useRef(onAttachFailed);
  const applyFitRef = useRef<() => void>(() => {});
  const instanceRef = useRef(instance);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [preview, setPreview] = useState("");
  const [rawTail, setRawTail] = useState("");
  const [ready, setReady] = useState(false);
  const frozen = status === "reconnecting" || status === "failed";

  const send = (data: string | Uint8Array) => {
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
    directRef.current = directInput;
  }, [directInput]);

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
      scrollback: 4000,
      convertEol: false,
      disableStdin: true,
      macOptionIsMeta: true,
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
    const outQueue: Uint8Array[] = [];
    let outRaf = 0;
    let resizeTimer = 0;

    const flushOut = () => {
      outRaf = 0;
      if (!outQueue.length) return;
      const merged = concatQueued(outQueue.splice(0));
      term.write(merged, () => {
        setReady(true);
        setMouseMode(term.modes.mouseTrackingMode);
      });
    };

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
          window.clearTimeout(resizeTimer);
          resizeTimer = window.setTimeout(() => {
            void sessionRef.current?.resize(size.cols, size.rows);
          }, 40);
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
      window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(() => {
        void sessionRef.current?.resize(term.cols, term.rows);
      }, 40);
    };

    const inputDisposable = term.onData((data) => {
      if (term.options.disableStdin) return;
      send(data);
    });
    const binaryDisposable = term.onBinary((data) => {
      if (term.options.disableStdin) return;
      send(binaryStringToBytes(data));
    });

    applyFitRef.current = applyFit;
    void whenFontsReady().then(() => applyFitRef.current());
    applyFit();
    void attachTerminalRenderer(term).then((name) => {
      setRenderer(name);
      applyFitRef.current();
    });

    const session = openTtySession(instanceRef.current, {
      onSnapshot: () => {
        resetStreamRef.current = true;
        term.reset();
      },
      onFrame: (payload, _offset, _streamId, reset) => {
        const shouldReset = reset || resetStreamRef.current;
        if (shouldReset) {
          term.reset();
          resetStreamRef.current = false;
        }
        const bytes = payloadForStreamWrite(payload, false);
        const text = stripAnsi(payload);
        const latin1 = Array.from(payload, (b) => String.fromCharCode(b)).join("");
        setPreview((current) => (shouldReset ? text : current + text));
        setRawTail((current) => (shouldReset ? latin1 : (current + latin1).slice(-4000)));
        outQueue.push(bytes);
        if (!outRaf) outRaf = requestAnimationFrame(flushOut);
      },
      onStatus: (next, message) => {
        setStatus(next);
        if (next === "connecting") resetStreamRef.current = true;
        if (next === "failed") failRef.current?.(message ?? "tty follow failed");
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
      enabled: () => !(directRef.current && term.modes.mouseTrackingMode !== "none"),
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
      window.clearTimeout(resizeTimer);
      if (outRaf) cancelAnimationFrame(outRaf);
      inputDisposable.dispose();
      binaryDisposable.dispose();
      font.dispose();
      void session.detach();
      sessionRef.current = null;
      term.dispose();
      termRef.current = null;
    };
  }, [instance.id]);

  useEffect(() => {
    applyFitRef.current();
  }, [mode, fullscreen]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.disableStdin = frozen || !directInput;
    if (directInput && !frozen) term.focus();
  }, [directInput, frozen]);

  useEffect(() => {
    document.documentElement.dataset.ttyFullscreen = fullscreen ? "1" : "0";
    return () => {
      delete document.documentElement.dataset.ttyFullscreen;
    };
  }, [fullscreen]);

  return (
    <section
      className={css.lab}
      data-tty-lab="1"
      data-tty-ready={ready ? "1" : "0"}
      data-tty-status={status}
      data-tty-cols={cols}
      data-tty-rows={rows}
      data-tty-io={ioMode}
      data-tty-renderer={renderer}
      data-tty-mouse={mouseMode}
      data-tty-fullscreen={fullscreen ? "1" : "0"}
      style={{ paddingBottom: offsetTop ? 0 : undefined }}
    >
      <header className={css.toolbar}>
        <span className={css.geo} data-testid="tty-io-mode">
          {cols}×{rows} · {ioMode} · {renderer} · {mode}
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
        <span className={css.modePill} data-testid="tty-mode-pill">
          {ioMode}
        </span>
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
        <button
          type="button"
          className={css.geo}
          data-testid="tty-fullscreen"
          aria-pressed={fullscreen}
          onClick={() => setFullscreen((open) => !open)}
        >
          {fullscreen ? "退出全屏" : "全屏"}
        </button>
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
        <pre className={css.preview} data-testid="tty-raw-tail">
          {rawTail}
        </pre>
      </div>
      <div className={css.dock}>
        {directInput ? (
          <>
            <span className={css.dockLabel}>raw</span>
            <div className={css.directGhost}>直连开启中 —— 击键 / 鼠标 / 粘贴直接进 PTY</div>
            <div className={css.directGhostSend}>发送</div>
          </>
        ) : (
          <LocalInput disabled={frozen} mobile={mobile} onSend={send} />
        )}
      </div>
      {mobile ? <AuxKeys disabled={frozen} onKey={send} /> : null}
      {!mobile ? (
        <div className={css.note}>
          TTY 字节走 `/v1/follow?tty=1` binary envelope · 结构 tab 看同一 journal
        </div>
      ) : null}
    </section>
  );
}
