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
import {
  allowInput,
  inputGate,
  localWheelWanted,
  MOUSE_TRACKING_RESET,
  trackingActive,
} from "./mouseReports";
import { createReplayGuard, groupByOrigin, type OutChunk } from "./replayGuard";
import { applyStdinPolicy, stdinPolicy } from "./stdinPolicy";
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

/** The font the terminal is built with, and the ceiling every fit measures from. */
const BASE_FONT_SIZE = 14;

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
  const frozenRef = useRef(false);
  const gateRef = useRef({ keyboard: true, mouse: true });
  const localWheelRef = useRef(false);
  const altScreenRef = useRef(false);
  const { mobile, coarsePointer, offsetTop } = useWorkbenchViewport();
  const [inputOverride, setInputOverride] = useState<{ direct: boolean; mode: DisplayMode } | null>(null);
  const [status, setStatus] = useState<TtyStatus>("connecting");
  const [cols, setCols] = useState(80);
  const [rows, setRows] = useState(24);
  const [fullscreen, setFullscreen] = useState(false);
  const [renderer, setRenderer] = useState<TerminalRenderer>("dom");
  const [mouseMode, setMouseMode] = useState("none");
  // A2: sticky DECSET tracking. `mouseReports` is the user's escape hatch —
  // with it off, pointer reports are dropped and the wheel scrolls locally.
  const [mouseReports, setMouseReports] = useState(true);
  // D-028 §4.6: the Node tells us on attach whether a full-screen TUI owns the
  // display. `undefined` = not reported (older Node, or the raw-ring carrier
  // which cannot know), and the wheel behaviour is then unchanged.
  const [altScreen, setAltScreen] = useState<boolean | undefined>(undefined);
  // A3: a narrow *desktop* window is still a mouse+keyboard terminal. Only a
  // coarse pointer (no hardware keyboard) should default to the local dock.
  const directInput = inputOverride?.direct ?? !coarsePointer;
  const mode = inputOverride?.mode ?? (mobile ? "responsive" : "fit");
  const ioMode = directInput ? "raw" : "keys";
  const modeRef = useRef<DisplayMode>(mode);
  const failRef = useRef(onAttachFailed);
  const applyFitRef = useRef<() => void>(() => {});
  const settleFitRef = useRef<() => void>(() => {});
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
    frozenRef.current = frozen;
  }, [frozen]);
  useEffect(() => {
    gateRef.current = inputGate({ directInput, frozen, mouseReports });
  }, [directInput, frozen, mouseReports]);
  useEffect(() => {
    localWheelRef.current = localWheelWanted({ mouseMode, mouseReports, altScreen });
    altScreenRef.current = altScreen === true;
  }, [mouseMode, mouseReports, altScreen]);

  useEffect(() => {
    const host = hostRef.current;
    const viewport = viewportRef.current;
    if (!host || !viewport) return;

    // The Terminal is rebuilt whenever `instance.id` changes (sidebar A→B→A
    // keeps this component mounted), so the stdin policy has to be derived
    // here from the *current* refs — the [directInput, frozen] effect below
    // does not re-run when only the terminal instance changed.
    const initialStdin = stdinPolicy({ directInput: directRef.current, frozen: frozenRef.current });
    const term = new Terminal({
      allowProposedApi: true,
      cursorBlink: true,
      cursorStyle: "block",
      fontFamily: TERMINAL_FONT_FAMILY,
      fontSize: BASE_FONT_SIZE,
      lineHeight: 1,
      scrollback: 4000,
      convertEol: false,
      disableStdin: initialStdin.disableStdin,
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
    if (initialStdin.focus) term.focus();
    // Per-instance view state must not leak across a session switch.
    resetStreamRef.current = true;
    setReady(false);
    setStatus("connecting");
    setMouseMode(term.modes.mouseTrackingMode);
    setMouseReports(true);
    setPreview("");
    setRawTail("");
    const font = createFontMeasure(host, TERMINAL_FONT_FAMILY);
    let disposed = false;
    const outQueue: OutChunk[] = [];
    const replayGuard = createReplayGuard();
    let outRaf = 0;
    let resizeTimer = 0;

    const flushOut = () => {
      outRaf = 0;
      if (!outQueue.length) return;
      // One write per origin run, not one per batch: a frame can carry replayed
      // snapshot bytes and live bytes together, and the guard decision differs.
      for (const run of groupByOrigin(outQueue.splice(0))) {
        if (run.replay) replayGuard.enter();
        term.write(run.bytes, () => {
          // xterm runs this right after *this* chunk is parsed, in FIFO order,
          // so the guard drops exactly when the replayed bytes are done.
          if (run.replay) replayGuard.leave();
          setReady(true);
          setMouseMode(term.modes.mouseTrackingMode);
        });
      }
    };

    const applyFit = () => {
      if (!termRef.current || !viewportRef.current) return;
      // A4: xterm is mounted in `.host`, which carries 16px/20px padding inside
      // `.viewport`. Measuring `.viewport` overshot by that padding, so the
      // bottom row was clipped and `.viewport` grew its own scrollbar.
      // `clientHeight` still counts padding, so take it off explicitly.
      const pad = window.getComputedStyle(host);
      const padX = (parseFloat(pad.paddingLeft) || 0) + (parseFloat(pad.paddingRight) || 0);
      const padY = (parseFloat(pad.paddingTop) || 0) + (parseFloat(pad.paddingBottom) || 0);
      const bounds = {
        width: Math.max(0, host.clientWidth - padX),
        height: Math.max(0, host.clientHeight - padY),
        dpr: window.devicePixelRatio || 1,
        lineHeight: term.options.lineHeight || 1,
        letterSpacing: term.options.letterSpacing || 0,
      };
      if (modeRef.current === "responsive") {
        const size = responsiveTerminalSize(bounds, font.measure, BASE_FONT_SIZE);
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
      if (modeRef.current === "fit") {
        // Size the font against the grid the box yields at the BASE font, not
        // against the current grid. `fit.fit()` derives cols/rows from the font
        // in effect, so feeding those back into the font search is a ratchet:
        // each pass shrinks the font, which widens the grid, which shrinks the
        // font again — 13.2px drifted to 1.66px and 1426 columns across a
        // fullscreen toggle. Anchoring on the base size makes it idempotent.
        const target = responsiveTerminalSize(bounds, font.measure, BASE_FONT_SIZE);
        if (target) {
          const fitted = fittedTerminalFont(
            { ...bounds, cols: target.cols, rows: target.rows },
            font.measure,
            BASE_FONT_SIZE,
          );
          if (fitted != null && term.options.fontSize !== fitted) term.options.fontSize = fitted;
        }
      }
      fit.fit();
      // A4: FitAddon derives rows from its own CSS cell estimate, which can
      // round one row larger than the renderer actually paints; that extra row
      // then overflows `.host` and the bottom line is clipped. Trim against
      // the painted screen height.
      const screen = host.querySelector<HTMLElement>(".xterm-screen");
      if (screen && term.rows > 1) {
        const painted = screen.getBoundingClientRect().height;
        const cell = painted / term.rows;
        if (cell > 0 && painted > bounds.height) {
          const fits = Math.max(3, Math.floor(bounds.height / cell));
          if (fits < term.rows) term.resize(term.cols, fits);
        }
      }
      setCols(term.cols);
      setRows(term.rows);
      window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(() => {
        void sessionRef.current?.resize(term.cols, term.rows);
      }, 40);
    };

    // Gate keyboard and pointer independently. `disableStdin` cannot do this:
    // xterm drops mouse reports on the same flag, which is what made `keys`
    // mode (and the stale-flag bug) kill scroll and clicks along with typing.
    const inputDisposable = term.onData((data) => {
      if (term.options.disableStdin) return;
      // Answers the emulator generated while parsing replayed history: the app
      // that asked is gone, so these would land as junk on the shell prompt.
      if (replayGuard.active()) return;
      if (!allowInput(data, gateRef.current)) return;
      send(data);
    });
    const binaryDisposable = term.onBinary((data) => {
      if (term.options.disableStdin) return;
      if (replayGuard.active()) return;
      if (!allowInput(data, gateRef.current)) return;
      send(binaryStringToBytes(data));
    });

    /**
     * Re-fit across a container size change that React drives (fullscreen,
     * display mode). One synchronous fit in the effect is not enough: the
     * fullscreen rules swap the container to `position: fixed; inset: 0;
     * height: 100dvh` plus safe-area padding, and the box the effect measures
     * can still be the pre-swap one — which is how a fullscreen terminal ends
     * up with a full-width but far-too-short grid drawn inside a tall area.
     * There is no CSS transition on `.lab`, so `transitionend` never fires and
     * cannot be the signal; settle across frames instead.
     */
    const settleFit = () => {
      applyFit();
      requestAnimationFrame(() => {
        if (disposed) return;
        applyFit();
        requestAnimationFrame(() => {
          if (disposed) return;
          applyFit();
        });
      });
      // Fonts can land after the transition, changing the cell size again.
      void whenFontsReady().then(() => {
        if (!disposed) applyFit();
      });
    };

    applyFitRef.current = applyFit;
    settleFitRef.current = settleFit;
    void whenFontsReady().then(() => applyFitRef.current());
    applyFit();
    void attachTerminalRenderer(term).then((name) => {
      // May resolve after a session switch already disposed this terminal.
      if (disposed) return;
      setRenderer(name);
      applyFitRef.current();
    });

    const session = openTtySession(instanceRef.current, {
      onSnapshot: () => {
        resetStreamRef.current = true;
        term.reset();
      },
      onFrame: (payload, _offset, _streamId, reset, replay) => {
        const shouldReset = reset || resetStreamRef.current;
        if (shouldReset) {
          term.reset();
          resetStreamRef.current = false;
        }
        const bytes = payloadForStreamWrite(payload, false);
        const text = stripAnsi(payload);
        const latin1 = Array.from(payload, (b) => String.fromCharCode(b)).join("");
        // B5: bound the preview like rawTail — an unbounded <pre> wedges long sessions.
        setPreview((current) => (shouldReset ? text : (current + text).slice(-4000)));
        setRawTail((current) => (shouldReset ? latin1 : (current + latin1).slice(-4000)));
        outQueue.push({ bytes, replay });
        if (!outRaf) outRaf = requestAnimationFrame(flushOut);
      },
      onStatus: (next, message) => {
        setStatus(next);
        if (next === "connecting") resetStreamRef.current = true;
        if (next === "failed") failRef.current?.(message ?? "tty follow failed");
      },
      onAltScreen: setAltScreen,
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

    // B5: `.xterm-rows > div` does not exist under the webgl/canvas renderers,
    // so the old lookup always fell back to a hardcoded 16px. Measure the
    // screen element instead — it is renderer-independent.
    const lineHeight = () => {
      const screen = host.querySelector<HTMLElement>(".xterm-screen");
      const measured = screen && term.rows > 0 ? screen.getBoundingClientRect().height / term.rows : 0;
      return measured > 0 ? measured : 16;
    };
    const scrollPixels = (deltaY: number) => {
      const lines = Math.trunc(deltaY / lineHeight());
      if (lines) term.scrollLines(lines);
    };

    const detachTouch = attachTerminalTouch(viewport, {
      onScrollPixels: scrollPixels,
      getGeneration: () => generationRef.current,
      hasSelection: () => term.hasSelection(),
      // With reports off we scroll locally, so touch panning stays ours —
      // unless a full-screen TUI owns the display, where there is no
      // scrollback to pan through (§4.6).
      enabled: () =>
        !altScreenRef.current
        && (localWheelRef.current || !trackingActive(term.modes.mouseTrackingMode)),
    });

    // A2: while the app has tracking on, xterm cancels the local wheel and
    // reports instead. With 鼠标上报 off we take the wheel back and scroll the
    // scrollback ourselves; the report itself is dropped by the input gate.
    term.attachCustomWheelEventHandler((event) => {
      if (!localWheelRef.current) return true;
      scrollPixels(-event.deltaY);
      event.preventDefault();
      return false;
    });

    window.__ttyLab = {
      disconnect: () => session.disconnectForTest(),
      reconnect: () => {
        resetStreamRef.current = true;
        session.reconnectForTest();
      },
    };

    return () => {
      disposed = true;
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

  // A container size change that React drives needs more than the one
  // synchronous fit this effect used to do — see settleFit. But only on an
  // actual transition: running the multi-frame settle on mount races the
  // renderer's own first sizing and leaves `.xterm-screen` collapsed.
  const lastLayoutRef = useRef(`${mode}:${fullscreen}`);
  useEffect(() => {
    const key = `${mode}:${fullscreen}`;
    const changed = lastLayoutRef.current !== key;
    lastLayoutRef.current = key;
    if (changed) settleFitRef.current();
    else applyFitRef.current();
  }, [mode, fullscreen]);

  useEffect(() => {
    applyStdinPolicy(termRef.current, { directInput, frozen });
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
      data-tty-mouse-reports={mouseReports ? "1" : "0"}
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
        {/* A2: sticky DECSET escape hatch. Only meaningful while the app asks
            for reports, which is exactly when the wheel stops scrolling. */}
        {trackingActive(mouseMode) ? (
          <>
            <button
              type="button"
              className={mouseReports ? css.segOn : css.geo}
              data-testid="tty-mouse-reports"
              aria-pressed={mouseReports}
              title="关闭后滚轮在本地滚动，不再把鼠标事件发给远端"
              onClick={() => setMouseReports((on) => !on)}
            >
              鼠标上报
            </button>
            <button
              type="button"
              className={css.geo}
              data-testid="tty-mouse-reset"
              title="向远端发送 DECRST，清掉残留的鼠标跟踪模式"
              onClick={() => {
                // Both halves are needed. Writing locally clears the emulator
                // even when the app that set `?1000h` is long gone (the common
                // sticky case, where nothing downstream would ever send the
                // DECRST back). Sending it on tells an app that *is* still
                // tracking to stop, so it does not immediately re-arm.
                const term = termRef.current;
                term?.write(MOUSE_TRACKING_RESET, () => setMouseMode(term.modes.mouseTrackingMode));
                send(MOUSE_TRACKING_RESET);
                setMouseReports(true);
              }}
            >
              重置终端模式
            </button>
          </>
        ) : null}
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
      {/* 直连 sends keys straight to the PTY, so the local input box has no
          job. Render nothing at all rather than a disabled ghost: `.dock`
          itself carries padding and a border-top, so keeping the container
          would still reserve a strip of dead space under the terminal. */}
      {!directInput ? (
        <div className={css.dock} data-testid="tty-dock">
          <LocalInput disabled={frozen} mobile={mobile} onSend={send} />
        </div>
      ) : null}
      {mobile ? <AuxKeys disabled={frozen} onKey={send} /> : null}
      {!mobile ? (
        <div className={css.note}>
          TTY 字节走 `/v1/follow?tty=1` binary envelope · 结构 tab 看同一 journal
        </div>
      ) : null}
    </section>
  );
}
