import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { useWorkbenchViewport } from "../../../lib/viewport";
import { hubStore } from "../../../lib/store";
import type { Instance } from "../../../types/instance";
import { payloadForStreamWrite, stripAnsi } from "./applyFrame";
import { AuxKeys } from "./AuxKeys";
import { PhoneKeyBar } from "./PhoneKeyBar";
import {
  clearTtyScrollLine,
  consumeTtyScrollLine,
  peekTtyScrollLine,
} from "./ttyScrollMemory";
import { TuiModeIndicator } from "./TuiModeIndicator";
import { TtyProgressBar } from "./TtyProgressBar";
import {
  openTtySession,
  type TtyProgress,
  type TtySession,
  type TtyStale,
  type TtyStatus,
} from "./client";
import { binaryStringToBytes } from "./ids";
import { LocalInput } from "./LocalInput";
import { attachTerminalRenderer, type TerminalRenderer } from "./renderer";
import { probeRendererSelection } from "./rendererProbe";
import { profileRegion } from "../../../lib/profileFlags";
import {
  createFontMeasure,
  fittedTerminalFont,
  responsiveTerminalSize,
  rowsFittingHeight,
  whenFontsReady,
} from "./terminalFit";
import { attachTerminalTouch } from "./terminalTouch";
import {
  allowInput,
  inputGate,
  localWheelWanted,
  MOUSE_TRACKING_RESET,
  trackingActive,
} from "./mouseReports";
import { createReplayGuard, groupByOrigin, type OutChunk } from "./replayGuard";
import {
  createSizeReconciler,
  type Grid,
  type SizeReconciler,
} from "./sizeReconciler";
import { applyStdinPolicy, stdinPolicy } from "./stdinPolicy";
import { StaleScreenBadge } from "./StaleScreenBadge";
import { terminalThemeFor, TERMINAL_FONT_FAMILY } from "./theme";
import css from "./TerminalView.module.css";

export type TtyLabHandle = {
  disconnect: () => void;
  reconnect: () => void;
  // UO-10: PTY resize accounting, exposed on window.__ttyLab for the keyboard
  // freeze / rotation / mode-cycle assertions.
  resizeCount: () => number;
  resetResizeCount: () => void;
  /** Full scrollback text (UO-10 live-switch test). */
  bufferText: () => string;
  themeBackground: () => string;
  themeForeground: () => string;
  rendererName: () => string;
  bufferActive: () => {
    cursorY: number;
    baseY: number;
    viewportY: number;
  };
  // Grid of the most recent resize actually sent to the PTY (round-6
  // reconciler tests assert the PTY ends at the final xterm grid).
  lastResize: () => { cols: number; rows: number } | null;
  // UO-10 round-5: drive xterm's scrollback (fires onScroll like the wheel
  // and touch paths) for the frozen-crop re-anchor test.
  scrollLines: (n: number) => void;
  // UO-10 round-5: send raw bytes straight to the session (tty.write),
  // bypassing the local/keys input gate — deterministic fake-harness
  // sentinels without changing the terminal chrome.
  writeRaw: (data: string) => void;
};

declare global {
  interface Window {
    __ttyLab?: TtyLabHandle;
  }
}

type DisplayMode = "fit" | "fixed" | "responsive";

/** The font the terminal is built with, and the ceiling every fit measures from. */
const BASE_FONT_SIZE = 14;

type TerminalAppearance = "dark" | "light";

/**
 * Current terminal appearance. `data-appearance="light"` (settings choice)
 * forces light; otherwise ("dark", or absent = system) follow
 * prefers-color-scheme. Re-renders on settings changes AND system changes,
 * so the xterm theme can be swapped live without rebuilding the terminal.
 */
function useTerminalAppearance(): TerminalAppearance {
  const [appearance, setAppearance] = useState<TerminalAppearance>(() => {
    if (typeof window === "undefined" || !window.matchMedia) return "dark";
    const stamped = document.documentElement.dataset.appearance;
    if (stamped === "light" || stamped === "dark") return stamped;
    return window.matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
  });
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: light)");
    const compute = () => {
      const stamped = document.documentElement.dataset.appearance;
      if (stamped === "light" || stamped === "dark") {
        setAppearance(stamped);
        return;
      }
      setAppearance(media.matches ? "light" : "dark");
    };
    compute();
    media.addEventListener("change", compute);
    const observer = new MutationObserver(compute);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-appearance"],
    });
    return () => {
      media.removeEventListener("change", compute);
      observer.disconnect();
    };
  }, []);
  return appearance;
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
  const frozenRef = useRef(false);
  const gateRef = useRef({ keyboard: true, mouse: true });
  const localWheelRef = useRef(false);
  const altScreenRef = useRef(false);
  const { mobile, coarsePointer, offsetTop } = useWorkbenchViewport();
  // UO-10 change of direction: the terminal FOLLOWS the workbench appearance.
  // Resolve "system" live (prefers-color-scheme) and react to explicit
  // settings changes that stamp data-appearance.
  const terminalAppearance = useTerminalAppearance();
  // Ref form for the one-shot mount effect (which must not rebuild the
  // terminal on appearance change).
  const terminalAppearanceRef = useRef(terminalAppearance);
  const [inputOverride, setInputOverride] = useState<{
    direct: boolean;
    mode: DisplayMode;
  } | null>(null);
  const [status, setStatus] = useState<TtyStatus>("connecting");
  // Present only with a `stale` status: how old the painted frame is and why
  // it stopped. Cleared by any live frame.
  const [stale, setStale] = useState<TtyStale | null>(null);
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
  // OSC 9;4 header progress; null = hidden (no progress / state 0).
  const [progress, setProgress] = useState<TtyProgress | null>(null);
  const [hasEngagedAltScreen, setHasEngagedAltScreen] = useState(false);
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
  // c-mkeybar: the 史 key fills a chosen previous prompt into the local input
  // strip. A new nonce remounts LocalInput with the text — fill never sends
  // (D-028a write boundary).
  const [inputFill, setInputFill] = useState<{
    text: string;
    nonce: number;
  } | null>(null);
  const [preview, setPreview] = useState("");
  const [rawTail, setRawTail] = useState("");
  const [ready, setReady] = useState(false);
  // A stale frame is the Hub's cache: `tty.attach` could not be answered, so
  // keystrokes have nowhere to go. Freezing it is the same call the reconnect
  // path already makes — `disableStdin` is an all-or-nothing switch, and
  // leaving it on would type into whatever is at the prompt *now* while the
  // operator reads a screen from an hour ago.
  const frozen =
    status === "reconnecting" || status === "failed" || status === "stale";

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
  // Live theme switch: assign the palette on the EXISTING Terminal so the
  // WebGL/canvas renderer repaints in place — no rebuild, no scrollback loss.
  useEffect(() => {
    terminalAppearanceRef.current = terminalAppearance;
    if (termRef.current)
      termRef.current.options.theme = terminalThemeFor(terminalAppearance);
  }, [terminalAppearance]);
  useEffect(() => {
    gateRef.current = inputGate({ directInput, frozen, mouseReports });
  }, [directInput, frozen, mouseReports]);
  useEffect(() => {
    localWheelRef.current = localWheelWanted({
      mouseMode,
      mouseReports,
      altScreen,
    });
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
    const initialStdin = stdinPolicy({
      directInput: directRef.current,
      frozen: frozenRef.current,
    });
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
      theme: terminalThemeFor(terminalAppearanceRef.current),
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
    setStale(null);
    setMouseMode(term.modes.mouseTrackingMode);
    setMouseReports(true);
    setAltScreen(undefined);
    setProgress(null);
    setHasEngagedAltScreen(false);
    setPreview("");
    setRawTail("");
    const font = createFontMeasure(host, TERMINAL_FONT_FAMILY);
    let disposed = false;
    const outQueue: OutChunk[] = [];
    const replayGuard = createReplayGuard();
    let outRaf = 0;
    // UO-10 round-6: ONE size reconciler owns the grid. It tracks lastSent
    // (what the PTY was told) and keyboardHold (rows pinned while the soft
    // keyboard is open), applies every recomputed desired grid to xterm
    // immediately, and debounces a single PTY send. No freeze/defer/fit gate,
    // no rotation flag, no frozen snapshot, no per-event cancellation — see
    // sizeReconciler.ts. Constructed after measureDesired/applyGrid below.
    let reconciler!: SizeReconciler;
    /**
     * UO-10 round-4 item 4: while frozen, position the pre-keyboard-sized
     * host inside the clipped viewport so the PAINTED cursor row (which
     * follows the user's scroll position) stays fully visible.
     *
     * The painted row is baseY + cursorY - viewportY (the row of the active
     * line in the on-screen grid); when the user has scrolled back it can
     * lie outside 0..rows-1, in which case no translate (offset 0) is
     * applied — the frozen top already shows the scrolled region.
     *
     * The translate also accounts for the host's TOP PADDING (12px on
     * phones): the screen starts padTop inside the host, so a cursor on the
     * last grid row sits padTop + rows*cellH from the host's top edge and
     * must be pulled into the viewport including that padding.
     */
    const updateFreezeCrop = () => {
      if (document.documentElement.dataset.keyboard !== "1") {
        host.style.transform = "";
        return;
      }
      const screen = host.querySelector<HTMLElement>(".xterm-screen");
      const screenH = screen?.getBoundingClientRect().height ?? 0;
      const cellH = term.rows > 0 ? screenH / term.rows : 0;
      const viewportH = viewport.clientHeight;
      const padTop = parseFloat(getComputedStyle(host).paddingTop) || 0;
      if (cellH <= 0 || viewportH <= 0) return;
      const visibleRows = (viewportH - padTop) / cellH;
      const active = term.buffer.active;
      const paintedRow = active.baseY + active.cursorY - active.viewportY;
      // Scrolled back outside the grid window: show the top of the host.
      if (paintedRow < 0 || paintedRow > term.rows - 1) {
        host.style.transform = "translateY(0px)";
        return;
      }
      const maxOffset = Math.max(0, term.rows - visibleRows);
      const offsetRows = Math.min(
        maxOffset,
        Math.max(0, paintedRow - visibleRows + 1),
      );
      host.style.transform = `translateY(${-Math.round(offsetRows * cellH)}px)`;
    };

    const flushOut = () => {
      outRaf = 0;
      if (!outQueue.length) return;
      // One write per origin run, not one per batch: a frame can carry replayed
      // snapshot bytes and live bytes together, and the guard decision differs.
      for (const run of groupByOrigin(outQueue.splice(0))) {
        if (run.replay) replayGuard.enter();
        // c-perfaudit: xterm parse/dispatch cost per flushed run (region is a
        // no-op without ?profile=1).
        profileRegion("tty.termWrite", () => {
          term.write(run.bytes, () => {
            // xterm runs this right after *this* chunk is parsed, in FIFO order,
            // so the guard drops exactly when the replayed bytes are done.
            if (run.replay) replayGuard.leave();
            setReady(true);
            setMouseMode(term.modes.mouseTrackingMode);
            // UO-10: keep the frozen crop anchored to the cursor as output
            // arrives while the keyboard is open.
            if (document.documentElement.dataset.keyboard === "1")
              updateFreezeCrop();
          });
        });
      }
    };

    /**
     * Measure the RAW grid the current container + mode want. This knows nothing
     * about the keyboard hold: cols always follow the real width and rows follow the
     * real (possibly keyboard-shrunk) height. The reconciler overrides rows with
     * keyboardHold while the keyboard is up, and fit/fixed never re-search the
     * font while held — so a rotation while the keyboard is open changes COLS to
     * the new width and keeps the held rows, exactly once.
     */
    const measureDesired = (): Grid | null => {
      if (!termRef.current || !viewportRef.current) return null;
      // A4: xterm is mounted in `.host`, which carries 16px/20px padding inside
      // `.viewport`. `clientHeight` counts padding, so take it off explicitly.
      const pad = window.getComputedStyle(host);
      const padX =
        (parseFloat(pad.paddingLeft) || 0) +
        (parseFloat(pad.paddingRight) || 0);
      const padY =
        (parseFloat(pad.paddingTop) || 0) +
        (parseFloat(pad.paddingBottom) || 0);
      const width = Math.max(0, host.clientWidth - padX);
      const height = Math.max(0, host.clientHeight - padY);
      const holding = reconciler.holdRows() !== null;
      const bounds = {
        width,
        height,
        dpr: window.devicePixelRatio || 1,
        lineHeight: term.options.lineHeight || 1,
        letterSpacing: term.options.letterSpacing || 0,
      };
      if (bounds.width <= 0 || bounds.height <= 0) return null;
      if (modeRef.current === "responsive") {
        return responsiveTerminalSize(
          bounds,
          font.measure,
          BASE_FONT_SIZE,
        );
      }
      // `fit` is the only mode that re-sizes the FONT. Anchor it against the
      // grid the box yields at the BASE font, not the current grid:
      // `proposeDimensions` derives cols/rows from the font in effect, so feeding
      // those back into the font search is a ratchet (each pass shrinks the
      // font, widens the grid, shrinks the font again — 13.2px once drifted
      // to 1.66px). `fixed` keeps whatever font is in effect and only refits
      // the grid.
      if (modeRef.current === "fit" && !holding) {
        const target = responsiveTerminalSize(
          bounds,
          font.measure,
          BASE_FONT_SIZE,
        );
        if (target) {
          const fitted = fittedTerminalFont(
            { ...bounds, cols: target.cols, rows: target.rows },
            font.measure,
            BASE_FONT_SIZE,
          );
          if (fitted != null && term.options.fontSize !== fitted)
            term.options.fontSize = fitted;
        }
      }
      const proposal = fit.proposeDimensions();
      const fallback = responsiveTerminalSize(
        bounds,
        font.measure,
        term.options.fontSize ?? BASE_FONT_SIZE,
      );
      let cols = proposal?.cols ?? fallback?.cols ?? term.cols;
      let rows = proposal?.rows ?? fallback?.rows ?? term.rows;
      // A4: FitAddon derives rows from its own CSS cell estimate, which can
      // round one row larger than the renderer actually paints; that extra row
      // overflows `.host` and the bottom line is clipped. The cell comes from
      // the grid CURRENTLY painted (painted / term.rows), never from the new
      // proposal — painted/proposalRows rescales the cell on a shrink and
      // applies the height loss twice (see rowsFittingHeight).
      const screen = host.querySelector<HTMLElement>(".xterm-screen");
      if (screen && term.rows > 0) {
        const painted = screen.getBoundingClientRect().height;
        rows = rowsFittingHeight(rows, painted / term.rows, height);
      }
      return { cols, rows };
    };

    const applyGrid = (grid: Grid) => {
      if (term.cols !== grid.cols || term.rows !== grid.rows)
        term.resize(grid.cols, grid.rows);
      setCols(grid.cols);
      setRows(grid.rows);
      // The crop is visual only; keep it anchored to the freshly applied grid.
      if (document.documentElement.dataset.keyboard === "1") updateFreezeCrop();
    };

    reconciler = createSizeReconciler({
      currentGrid: () => ({ cols: term.cols, rows: term.rows }),
      measureDesired,
      applyGrid,
      sendResize: (grid) => {
        void sessionRef.current?.resize(grid.cols, grid.rows);
      },
    });

    // Every layout signal (ResizeObserver, visualViewport/window resize,
    // rotation, mode/fullscreen change, font load) goes through this one call.
    const applyFit = () => reconciler.layoutChanged();

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
    // The terminal can mount while the keyboard is already up (deep-link from a
    // focused input): open the hold BEFORE the first fit so its rows pin the
    // pre-keyboard grid (the PTY's initial 80x24 until the first send),
    // instead of a grid measured from the shrunken band.
    if (document.documentElement.dataset.keyboard === "1") {
      reconciler.keyboardOpened();
      updateFreezeCrop();
    }
    void whenFontsReady().then(() => applyFitRef.current());
    applyFit();
    // c-mfix: a later WebGL context loss (iOS keyboard/memory pressure is a
    // common trigger) downgrades to canvas; keep the pill and the grid fit in
    // sync with the renderer actually painting.
    const onEffectiveRenderer = (name: TerminalRenderer) => {
      if (disposed) return;
      probeRendererSelection(name);
      setRenderer(name);
      applyFitRef.current();
    };
    void attachTerminalRenderer(term, onEffectiveRenderer).then((name) => {
      // May resolve after a session switch already disposed this terminal.
      if (disposed) return;
      onEffectiveRenderer(name);
    });

    const session = openTtySession(instanceRef.current, {
      onSnapshot: () => {
        resetStreamRef.current = true;
        term.reset();
      },
      onFrame: (payload, _offset, _streamId, reset, replay) => {
        profileRegion("tty.onFrame", () => {
          const shouldReset = reset || resetStreamRef.current;
          if (shouldReset) {
            term.reset();
            resetStreamRef.current = false;
          }
          const bytes = payloadForStreamWrite(payload, false);
          const text = stripAnsi(payload);
          const latin1 = Array.from(payload, (b) =>
            String.fromCharCode(b),
          ).join("");
          // B5: bound the preview like rawTail — an unbounded <pre> wedges long sessions.
          setPreview((current) =>
            shouldReset ? text : (current + text).slice(-4000),
          );
          setRawTail((current) =>
            shouldReset ? latin1 : (current + latin1).slice(-4000),
          );
          outQueue.push({ bytes, replay });
          if (!outRaf) outRaf = requestAnimationFrame(flushOut);
        });
      },
      onStatus: (next, message, nextStale) => {
        setStatus(next);
        setStale(next === "stale" ? (nextStale ?? {}) : null);
        if (next === "connecting") {
          resetStreamRef.current = true;
          setAltScreen(undefined);
          setProgress(null);
        }
        if (next === "failed")
          failRef.current?.(message ?? "tty follow failed");
      },
      onAltScreen: (active) => {
        // Undefined explicitly invalidates an older observation when the
        // replacement attach has no emulator-backed mode evidence.
        setAltScreen(active);
        if (active === true) setHasEngagedAltScreen(true);
      },
      onProgress: setProgress,
    });
    sessionRef.current = session;
    generationRef.current += 1;

    const observer = new ResizeObserver(() => {
      applyFit();
    });
    observer.observe(viewport);
    const onViewport = () => {
      if (window.visualViewport && window.visualViewport.scale !== 1) return;
      // UO-10 round-7: the keyboard detector (lib/viewport) registered its
      // listener before this effect's, so on THIS same event it has already
      // stamped data-keyboard AND applied the shrunken --workbench-height.
      // Sync the hold BEFORE measuring: onset must capture the grid still on
      // screen (a legitimate resize pending in the debounce goes out once),
      // while this event's own height loss is measured immediately after and
      // gets pinned. Measuring first would capture the keyboard-shrunk grid
      // and freeze the PTY down to the band (or send it).
      if (document.documentElement.dataset.keyboard === "1")
        reconciler.keyboardOpened();
      else reconciler.keyboardClosed();
      applyFit();
    };
    // Soft-keyboard edges via attribute transitions not delivered through the
    // resize handlers above (mount-with-keyboard, focus-only stamps). Opening
    // captures rows; closing releases them. The viewport path syncs the hold
    // before its own measure, so this observer's reconcile is redundant there
    // (both calls are idempotent) and only handles the extra cases.
    const keyboardObserver = new MutationObserver(() => {
      if (document.documentElement.dataset.keyboard === "1") {
        reconciler.keyboardOpened();
        // Visual only (never touches the grid): re-anchor the cursor crop now,
        // not only on the next output frame.
        updateFreezeCrop();
      } else {
        reconciler.keyboardClosed();
        host.style.transform = "";
      }
    });
    keyboardObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-keyboard"],
    });
    // UO-10 round-5 item 3: native wheel/scrollbar scroll changes the
    // painted rows; re-anchor the frozen crop to the scrolled view.
    const onTermScroll = () => updateFreezeCrop();
    term.onScroll(onTermScroll);
    window.visualViewport?.addEventListener("resize", onViewport);
    window.addEventListener("resize", onViewport);

    // B5: `.xterm-rows > div` does not exist under the webgl/canvas renderers,
    // so the old lookup always fell back to a hardcoded 16px. Measure the
    // screen element instead — it is renderer-independent.
    const lineHeight = () => {
      const screen = host.querySelector<HTMLElement>(".xterm-screen");
      const measured =
        screen && term.rows > 0
          ? screen.getBoundingClientRect().height / term.rows
          : 0;
      return measured > 0 ? measured : 16;
    };
    const scrollPixels = (deltaY: number) => {
      const lines = Math.trunc(deltaY / lineHeight());
      if (lines) {
        term.scrollLines(lines);
        // UO-10 round-5 item 3: keep the frozen crop anchored to what the
        // user scrolled to, not the live cursor.
        updateFreezeCrop();
      }
    };

    const detachTouch = attachTerminalTouch(viewport, {
      onScrollPixels: scrollPixels,
      getGeneration: () => generationRef.current,
      hasSelection: () => term.hasSelection(),
      // With reports off we scroll locally, so touch panning stays ours —
      // unless a full-screen TUI owns the display, where there is no
      // scrollback to pan through (§4.6).
      enabled: () =>
        !altScreenRef.current &&
        (localWheelRef.current ||
          !trackingActive(term.modes.mouseTrackingMode)),
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
      // PTY resize calls since the last reset — the keyboard freeze test asserts
      // zero across an open/close cycle.
      resizeCount: () => reconciler.resizeCount(),
      resetResizeCount: () => reconciler.resetCount(),
      // UO-10 round-4: live-switch test support.
      bufferText: () => {
        const b = term.buffer.active;
        let out = "";
        for (let i = 0; i < b.length; i++) {
          const line = b.getLine(i);
          if (line) out += line.translateToString(true) + "\n";
        }
        return out;
      },
      themeBackground: () => (term.options.theme?.background as string) ?? "",
      themeForeground: () => (term.options.theme?.foreground as string) ?? "",
      rendererName: () =>
        termRef.current?.element?.querySelector("canvas")
          ? "canvas-or-webgl"
          : "dom",
      bufferActive: () => {
        const b = term.buffer.active;
        return {
          cursorY: b.cursorY,
          baseY: b.baseY,
          viewportY: b.viewportY,
        };
      },
      lastResize: () => reconciler.lastSent(),
      scrollLines: (n: number) => term.scrollLines(n),
      writeRaw: (data: string) => {
        void sessionRef.current?.write(data);
      },
    };

    return () => {
      disposed = true;
      delete window.__ttyLab;
      detachTouch();
      observer.disconnect();
      keyboardObserver.disconnect();
      reconciler.dispose();
      window.visualViewport?.removeEventListener("resize", onViewport);
      window.removeEventListener("resize", onViewport);
      host.style.transform = "";
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

  // c-mkeybar: restore the scroll line remembered by the nine-key git key.
  // The fresh attach replays the Hub screen snapshot, so the buffer builds a
  // frame at a time — retry until the remembered line exists, then consume
  // the memory exactly once. xterm v6 scrolls virtually (DOM scrollTop is
  // inert), so the restore goes through the terminal scroll API.
  useEffect(() => {
    if (!ready) return;
    if (peekTtyScrollLine(instance.id) == null) return;
    let timer = 0;
    let tries = 0;
    const apply = () => {
      const term = termRef.current;
      const line = term
        ? consumeTtyScrollLine(
            instance.id,
            term.buffer.active.length,
            term.rows,
          )
        : null;
      if (line != null) {
        term!.scrollToLine(line);
        return;
      }
      if (term && ++tries >= 80) clearTtyScrollLine(instance.id);
      else timer = window.setTimeout(apply, 50);
    };
    apply();
    return () => window.clearTimeout(timer);
  }, [ready, instance.id]);

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
      data-tty-instance={instance.id}
      data-tty-ready={ready ? "1" : "0"}
      data-tty-status={status}
      data-tty-cols={cols}
      data-tty-rows={rows}
      data-tty-io={ioMode}
      data-tty-renderer={renderer}
      data-tty-mouse={mouseMode}
      data-tty-mouse-reports={mouseReports ? "1" : "0"}
      data-tty-fullscreen={fullscreen ? "1" : "0"}
      data-tty-alt-screen={
        altScreen === undefined ? "unknown" : String(altScreen)
      }
      data-tty-progress={progress ? progress.state : "hidden"}
      style={{ paddingBottom: offsetTop ? 0 : undefined }}
    >
      <header className={css.toolbar}>
        <span className={css.geo} data-testid="tty-io-mode">
          {cols}×{rows} · {ioMode} · {renderer} · {mode}
        </span>
        <TuiModeIndicator
          altScreen={altScreen}
          requestedTui={instance.kind === "claude" ? instance.tui : undefined}
          hasEngagedAltScreen={hasEngagedAltScreen}
        />
        {/* The header strip's honesty: a stale frame must say so here, beside
            the renderer pill, and not only in the banner below — 运行中 alone
            over a frozen screen is the failure this batch exists to end. */}
        {status === "stale" && stale ? (
          <StaleScreenBadge stale={stale} />
        ) : null}
        <div className={css.seg}>
          <button
            type="button"
            className={!directInput ? css.segOn : undefined}
            aria-pressed={!directInput}
            onClick={() =>
              setInputOverride((current) => ({
                direct: false,
                mode: current?.mode ?? mode,
              }))
            }
          >
            本地输入
          </button>
          <button
            type="button"
            className={directInput ? css.segOn : undefined}
            aria-pressed={directInput}
            onClick={() =>
              setInputOverride((current) => ({
                direct: true,
                mode: current?.mode ?? mode,
              }))
            }
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
                term?.write(MOUSE_TRACKING_RESET, () =>
                  setMouseMode(term.modes.mouseTrackingMode),
                );
                send(MOUSE_TRACKING_RESET);
                setMouseReports(true);
              }}
            >
              重置终端模式
            </button>
          </>
        ) : null}
        {!mobile ? (
          <AuxKeys disabled={frozen} onKey={send} variant="toolbar" />
        ) : null}
        {!mobile ? (
          <button
            type="button"
            className={css.geo}
            onClick={() =>
              setInputOverride((current) => {
                // Derive from the QUEUED override, not the render-scope
                // `mode`: two clicks landing in one React batch (or
                // programmatic dispatches) must cycle twice, not twice
                // compute from the same stale render.
                const currentMode = current?.mode ?? mode;
                const next =
                  currentMode === "fit"
                    ? "fixed"
                    : currentMode === "fixed"
                      ? "responsive"
                      : "fit";
                return { direct: current?.direct ?? directInput, mode: next };
              })
            }
          >
            {mode}
          </button>
        ) : null}
        {!mobile ? (
          <button
            type="button"
            className={css.geo}
            aria-pressed={searchOpen}
            onClick={() => setSearchOpen((open) => !open)}
          >
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
            <input
              className={css.searchInput}
              aria-label="搜索终端"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
            />
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
      <TtyProgressBar progress={progress} />
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
              : status === "stale"
                ? "画面来自 Hub 缓存，不是实时输出"
                : "终端连接失败"}
          {status === "reconnecting" ||
          status === "failed" ||
          status === "stale" ? (
            <button
              type="button"
              className={css.geo}
              onClick={() => {
                resetStreamRef.current = true;
                // A stale frame has an open socket (that is how the cached
                // bytes arrived), so `reconnectForTest` would return without
                // doing anything. Re-asking the Hub is the retry that can
                // actually replace the fossil.
                if (status === "stale") sessionRef.current?.retrySnapshot();
                else sessionRef.current?.reconnectForTest();
              }}
            >
              重连
            </button>
          ) : null}
        </div>
      ) : null}
      <div
        className={css.viewport}
        ref={viewportRef}
        role="region"
        aria-label="终端画面"
      >
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
          {/* D-028 §5.2: the dock routes through instance.send so the driver
              performs body-then-Enter as two PTY writes; raw key buttons
              below stay on the binary channel. */}
          <LocalInput
            key={inputFill?.nonce ?? 0}
            initialText={inputFill?.text ?? ""}
            disabled={frozen}
            mobile={mobile}
            onSend={(text) => void hubStore.send(instance.id, text)}
          />
        </div>
      ) : null}
      {mobile ? (
        <PhoneKeyBar
          instance={instance}
          disabled={frozen}
          onKey={send}
          captureScrollLine={() => termRef.current?.buffer.active.baseY ?? 0}
          onFillInput={(text) => setInputFill({ text, nonce: Date.now() })}
        />
      ) : null}
      {!mobile ? (
        <div className={css.note}>
          TTY 字节走 `/v1/follow?tty=1` binary envelope · 结构 tab 看同一
          journal
        </div>
      ) : null}
    </section>
  );
}
