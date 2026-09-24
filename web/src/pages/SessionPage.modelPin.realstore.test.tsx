/**
 * model-pin-1 §5.4 acceptance: rendered regressions through the REAL store.
 *
 * No literal component props, no mocked runningModelOf, no hand-injected
 * hub state. Raw InstanceRecord JSON is served over the wire boundary
 * (global fetch -> instanceGet -> mapInstance), `hubStore.follow` and
 * `useHub` are the real ones, and model/diagnostic observations arrive
 * through the real eventsSubscribe delivery boundary. We then render the
 * real SessionPage (chip + run details) and SessionList (row chip).
 */
import { Component, type ReactNode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { hubStore } from "../lib/store";
import type { Observation } from "../types/observation";
import { SessionPage } from "../pages/SessionPage";
import { SessionsPage } from "../pages/SessionsPage";
import { spaceStore } from "../features/spaces/store";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
}));

type Deliver = (batch: {
  subscriptionId: string;
  journalId: string;
  fromSeq: string;
  toSeq: string;
  durableSeq: string;
  events: Observation[];
}) => void;

/** Minimal RAW wire InstanceRecord (pre-mapInstance) for one scenario. */
function wireRecord(over: Record<string, unknown>): Record<string, unknown> {
  return {
    instanceId: over.instanceId ?? "ins_pin_real",
    hostId: "hst_pin_real",
    workspaceId: "wsp_pin_real",
    kind: "claude",
    driver: "shell-pty",
    lifecycle: "ready",
    activity: "idle",
    connectivity: "connected",
    createdAt: "2026-09-24T00:00:00Z",
    updatedAt: "2026-09-24T00:00:00Z",
    title: "pin real",
    journalId: over.instanceId ?? "ins_pin_real",
    durableSeq: "1",
    // A live session: without a "known" exit the list row is hidden.
    exit: { state: "known", value: "idle" },
    ...over,
  };
}

function modelEvent(seq: number, id: string, source: string): Observation {
  return {
    eventId: `evt_m_${seq}`,
    instanceId: "x",
    journalId: "x",
    seq: String(seq),
    kind: "model",
    observedAt: `2026-09-24T00:0${seq}:00Z`,
    source: { channel: "transcript" },
    // launch edges name the requested id; slash/unknown do not.
    payload:
      source === "slash" || source === "unknown"
        ? { effective: { id, source, observedAt: `2026-09-24T00:0${seq}:00Z` }, raw: id }
        : {
            requested: id,
            effective: { id, source, observedAt: `2026-09-24T00:0${seq}:00Z` },
            raw: id,
          },
  } as unknown as Observation;
}

function mismatchEvent(seq: number, requested: string, observed: string): Observation {
  return {
    eventId: `evt_pin_${seq}`,
    instanceId: "x",
    journalId: "x",
    seq: String(seq),
    kind: "lifecycle",
    observedAt: `2026-09-24T00:0${seq}:00Z`,
    source: { channel: "runtime" },
    payload: {
      type: "native",
      topic: "diagnostic",
      nativeName: "model_pin_mismatch",
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: "diverged" },
      severity: "warning",
      affectsCompletion: false,
      dataRef: null,
      relatedIds: { reason: "model-mismatch", requested, observed },
    },
  } as unknown as Observation;
}

type Ctx = {
  id: string;
  /** Deliver one observation through the live subscription, in act(). */
  deliver: (observation: Observation) => Promise<void>;
  /** Observation events the bounded journal GET returns at follow time. */
  seed: Observation[];
};

/** A post-map browser Host (subset the store/list use) for one pin session. */
function hostSeed(hostId: string) {
  return {
    id: hostId,
    label: "pin-host",
    state: "online",
    transport: { mode: "outbound-wss" },
    capabilities: { apiRelay: false },
    workspaceRevision: 1,
  };
}

/** A registered workspace (the shape hostList carries under host.workspaces,
 *  which mapWorkspace consumes) for one pin session. */
function workspaceSeed(hostId: string, workspaceId: string) {
  return {
    hostId,
    workspaceId,
    root: "/home/pin/projects/pin-real",
  };
}

/**
 * Serve one raw instance record (+ its bounded journal tail) over the real
 * fetch boundary, and capture the live subscription. `follow`/`useHub` are
 * NOT mocked.
 */
async function setupRealSession(
  record: Record<string, unknown>,
  seed: Observation[] = [],
): Promise<Ctx> {
  const id = String(record.instanceId);
  const journalTail = {
    events: seed as unknown[],
    durableSeq: String(seed.length + 1),
    windowFromSeq: "1",
    reachedAfterSeq: true,
  };
  const fetchMock = vi.fn(async (input: string | URL | Request): Promise<Response> => {
    const url = String(input);
    if (url.endsWith(`/v1/instances/${id}`)) {
      return Response.json(record);
    }
    if (url.includes(`/v1/instances/${id}/journal`)) {
      return Response.json(journalTail);
    }
    // Everything else (screens, commands, …): a harmless empty OK.
    return Response.json({});
  });
  vi.stubGlobal("fetch", fetchMock);
  // The row renders inside a host/workspace space; hydrate those through the
  // normal public bootstrap ingestion (the host carries its registered
  // workspaces, exactly as the real /v1/hosts bootstrap maps them). This is
  // session context, not model state — the model data stays on the real
  // fetch -> instanceGet -> mapInstance path.
  const hostId = String(record.hostId);
  const workspaceId = String(record.workspaceId);
  vi.spyOn(api, "hostList").mockResolvedValue({
    items: [
      {
        ...hostSeed(hostId),
        workspaces: [workspaceSeed(hostId, workspaceId)],
      },
    ] as never,
    nextCursor: null,
  });
  // Avoid a real workspace websocket in jsdom; the bootstrap snapshot above
  // already populated the workspace.
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);

  let deliver!: Deliver;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_j, _a, onBatch) => {
    deliver = onBatch as Deliver;
    return {
      subscriptionId: `sub_${id}`,
      journalId: id,
      durableSeq: "1",
      windowFromSeq: "1",
      reachedAfterSeq: true,
      // follow() reconciles this snapshot with the bounded tail; carry the
      // already-mapped instance so the sub is accepted.
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_pin_real",
        asOfSeq: String(seed.length + 1),
        instance: null,
        runs: [],
        commands: [],
        pendingInteractions: [],
        nodes: [],
        history: { earliestRetainedSeq: "1", complete: true },
      },
    };
  });

  await hubStore.follow(id as never);
  // follow() hydrates the one instance; hosts/workspaces arrive through the
  // normal bootstrap host list — ingest them the same way a live session does.
  await hubStore.refreshHosts().catch(() => undefined);
  // follow() leaves the store "ready:false" (only the full bootstrap flips
  // ready); a live app always follows after bootstrap. Flip it the same way
  // so mounted pages don't sit behind the not-ready gate.
  (
    hubStore as unknown as {
      emit: (patch: { ready: boolean; authed: boolean; connection: "live" }) => void;
    }
  ).emit({ ready: true, authed: true, connection: "live" });
  const send = async (observation: Observation) => {
    await act(async () => {
      deliver({
        subscriptionId: `sub_${id}`,
        journalId: id,
        fromSeq: observation.seq,
        toSeq: observation.seq,
        durableSeq: observation.seq,
        events: [observation],
      });
      // Let the store's post-delivery microtasks (ack/gap) settle inside act.
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  };
  return {
    id,
    seed,
    deliver: send,
  };
}

function renderPage(id: string) {
  class Boundary extends Component<
    { children: ReactNode },
    { error: unknown }
  > {
    state = { error: null as unknown };
    static getDerivedStateFromError(error: unknown) {
      return { error };
    }
    override render() {
      return this.state.error ? (
        <pre data-testid="page-throw">{String((this.state.error as Error)?.stack ?? this.state.error)}</pre>
      ) : (
        this.props.children
      );
    }
  }
  return render(
    <MemoryRouter initialEntries={[`/s/${id}/structured`]}>
      <Boundary>
        <Routes>
          <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
          <Route path="*" element={<pre data-testid="route-nomatch">NOMATCH</pre>} />
        </Routes>
      </Boundary>
    </MemoryRouter>,
  );
}

function renderSessionsGlobal() {
  // Render ONLY the real sessions list (SessionsPage -> SessionList), in
  // global scope, reading the real hub.instances. Used by standalone list
  // cases (no prior SessionPage tree in the same test).
  render(
    <MemoryRouter initialEntries={["/sessions?scope=all"]}>
      <Routes>
        <Route path="/sessions" element={<SessionsPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  localStorage.clear();
  // spaceStore keeps in-memory prefs across tests in this file; reset it to
  // the (cleared-storage) defaults so an earlier test's selected space/order
  // cannot pin the list.
  spaceStore.reload();
});
afterEach(async () => {
  // Flush any store update the follow() subscription schedules just after
  // the last act() delivery (eventsAck/gap ticks), so a trailing scheduled
  // render cannot leak into the next test's React commit.
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  hubStore.logout();
});

describe("model-pin rendered through the real store (cases a-d)", () => {
  /** The model chip and its collapsed `effort-model` line live inside the
   *  effort popover; open it before asserting. */
  async function openModelChip() {
    const chip = await screen.findByTestId("model-effort-chip");
    fireEvent.click(chip);
    await screen.findByTestId("effort-model");
  }

  it("(a) launch A / read-back A: chip A, no diagnostic", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_a", model: "model_hub/A", kind: "claude" }),
    );
    renderPage(ctx.id);
    await openModelChip();
    expect(screen.getByTestId("effort-model")).toHaveTextContent("model_hub/A");
    await ctx.deliver(modelEvent(2, "model_hub/A", "launch"));
    expect(screen.getByTestId("effort-model")).toHaveTextContent("model_hub/A");
    expect(screen.queryAllByTestId("run-details-model-pin")).toHaveLength(0);
  });

  it("(b) launch A / read-back B: chip B, run details records requested A / observed B", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_b", model: "model_hub/A", kind: "claude" }),
    );
    renderPage(ctx.id);
    await openModelChip();
    await ctx.deliver(modelEvent(2, "model_hub/B", "launch"));
    await ctx.deliver(mismatchEvent(3, "model_hub/A", "model_hub/B"));
    expect(screen.getByTestId("effort-model")).toHaveTextContent("model_hub/B");
    const pin = screen.getByTestId("run-details-model-pin");
    expect(pin).toHaveTextContent("请求模型 model_hub/A，实际运行 model_hub/B");
    expect(pin).toHaveAttribute("data-requested", "model_hub/A");
    expect(pin).toHaveAttribute("data-observed", "model_hub/B");
  });

  it("(c) then /model C: chip C, the launch diagnostic stays in run details", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_c", model: "model_hub/A", kind: "claude" }),
    );
    renderPage(ctx.id);
    await openModelChip();
    await ctx.deliver(modelEvent(2, "model_hub/B", "launch"));
    await ctx.deliver(mismatchEvent(3, "model_hub/A", "model_hub/B"));
    await ctx.deliver(modelEvent(4, "model_hub/C", "slash"));
    expect(screen.getByTestId("effort-model")).toHaveTextContent("model_hub/C");
    // The launch divergence remains as history.
    const pin = screen.getByTestId("run-details-model-pin");
    expect(pin).toHaveTextContent("请求模型 model_hub/A，实际运行 model_hub/B");
  });

  it("(d) no launch model / read-back X: chip X, no invented default, no diagnostic", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_d", kind: "claude" }), // no model field
    );
    renderPage(ctx.id);
    await openModelChip();
    // Before read-back: a model-axis session with no launch model shows empty,
    // never an invented "opus".
    expect(screen.getByTestId("effort-model").textContent).toBe("");
    await ctx.deliver(modelEvent(2, "model_hub/X", "unknown"));
    await waitFor(() =>
      expect(screen.getByTestId("effort-model")).toHaveTextContent("model_hub/X"),
    );
    expect(screen.queryAllByTestId("run-details-model-pin")).toHaveLength(0);
  });

  it("codex model axis with neither launch spec nor read-back renders an empty chip", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_codex", kind: "codex", driver: "shell-pty" }),
    );
    renderPage(ctx.id);
    await openModelChip();
    // Codex has a model axis but no model id at all: the chip is empty, it
    // must not substitute the effort stop sentence nor a gpt-5 default.
    expect(screen.getByTestId("effort-model").textContent).toBe("");
    expect(screen.getByTestId("effort-model")).not.toHaveTextContent(/gpt|usage|agents/i);
  });

  it("a projected mismatch renders from the public record with an EMPTY event window", async () => {
    // The diagnostic has aged out of the bounded tail: follow GETs an empty
    // journal, but the raw instance JSON carries the durable projection
    // through mapInstance into the real store.
    const ctx = await setupRealSession(
      wireRecord({
        instanceId: "ins_pin_proj",
        model: "model_hub/es1_orange_o50[1m]",
        kind: "claude",
        modelPinMismatches: [
          {
            requested: "model_hub/es1_orange_o50[1m]",
            observed: "model_hub/es1_orange_o48[1m]",
            observedAt: "2026-09-24T00:00:00.000Z",
          },
        ],
      }),
      [], // empty bounded tail
    );
    renderPage(ctx.id);
    const pin = await screen.findByTestId("run-details-model-pin");
    expect(pin).toHaveTextContent(
      "请求模型 model_hub/es1_orange_o50[1m]，实际运行 model_hub/es1_orange_o48[1m]",
    );
    // Still one line even though no diagnostic event exists in the window.
    expect(screen.getAllByTestId("run-details-model-pin")).toHaveLength(1);
  });
});

describe("model-pin real SessionList rows (real store)", () => {
  it("(list) launch A / read-back B: the row chip shows running B, not a pair", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_list_b", model: "model_hub/A", kind: "claude" }),
    );
    await ctx.deliver(modelEvent(2, "model_hub/B", "launch"));
    renderSessionsGlobal();
    await screen.findByTestId("session-list");
    const chip = screen.getAllByTestId("session-model")[0];
    expect(chip.textContent).toBe("model_hub/B");
    expect(chip).not.toHaveTextContent("⇐");
  });

  it("(list) no launch model / read-back X: row shows X, nothing invented, slot present", async () => {
    const ctx = await setupRealSession(
      wireRecord({ instanceId: "ins_pin_list_d", kind: "claude" }),
    );
    await ctx.deliver(modelEvent(2, "model_hub/X", "unknown"));
    renderSessionsGlobal();
    await screen.findByTestId("session-list");
    const chip = screen.getAllByTestId("session-model")[0];
    expect(chip.textContent).toBe("model_hub/X");
    expect(chip).not.toHaveTextContent("opus");
  });

  it("(list) no launch model and no read-back: the model slot is omitted", async () => {
    await setupRealSession(wireRecord({ instanceId: "ins_pin_list_none", kind: "claude" }));
    renderSessionsGlobal();
    await screen.findByTestId("session-list");
    expect(screen.queryAllByTestId("session-model")).toHaveLength(0);
  });
});
