import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { DecisionCard, type DecisionView, type HomeCardState } from "./ApprovalCard";
import type { Interaction } from "../../types/interaction";
import { APPROVAL, PLAN, field, interaction, question } from "../home/queueTestData";

/**
 * `DecisionCard mode="home"` (home.md §4.2–4.4): one case per card kind.
 * Smaller surfaces grant less — allow-session is never offered and a plan is
 * never approvable on the home.
 */

/* ------------------------------------------------------------------ */
/* DecisionCard mode="home": one test per §4.2 card kind.             */
/* ------------------------------------------------------------------ */

function view(item: Interaction, uiState: DecisionView["uiState"] = "pending"): DecisionView {
  return {
    key: item.id,
    sig: `${item.id}:${uiState}`,
    item,
    uiState,
    focused: false,
    timeLabel: "1分",
    hostLabel: "forge",
    workspaceLabel: "remuda",
    instanceKind: "claude",
  };
}

function renderCard(item: Interaction, home: Partial<HomeCardState> = {}, uiState: DecisionView["uiState"] = "pending") {
  const onRespond = vi.fn();
  render(
    <MemoryRouter>
      <DecisionCard
        view={view(item, uiState)}
        mode="home"
        onRespond={onRespond}
        home={{ placement: "phone", receipt: null, stale: false, ...home }}
      />
    </MemoryRouter>,
  );
  return onRespond;
}

describe('DecisionCard mode="home" (§4.2, §4.3)', () => {
  it("renders only 拒绝 and 允许一次 for an approval — allow-session is never offered", () => {
    const onRespond = renderCard(interaction({ id: "int_a", instanceId: "ins_a" }));
    const buttons = screen.getAllByRole("button");
    expect(buttons.map((button) => button.textContent)).toEqual(["拒绝", "允许一次"]);
    expect(screen.queryByText(/始终允许/)).toBeNull();
    expect(screen.queryByText(/allow-session|持续允许/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "允许一次" }));
    expect(onRespond).toHaveBeenCalledWith(
      expect.objectContaining({ id: "int_a" }),
      { kind: "approval", optionId: "allow-once", inputDigest: "sha256:abab" },
    );
  });

  it("falls back to 去收件箱 only when allow-once is missing", () => {
    const request = { ...APPROVAL, options: APPROVAL.options.filter((opt) => opt.effect !== "allow-once") };
    renderCard(interaction({ id: "int_a", instanceId: "ins_a", request }));
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.getByTestId("home-card-inbox")).toHaveAttribute("href", "/m/inbox?focus=int_a");
  });

  it("renders a chip per option plus 其他… for a simple question", () => {
    const onRespond = renderCard(interaction({ id: "int_q", instanceId: "ins_a", request: question() }));
    expect(screen.getAllByTestId("home-card-chip").map((chip) => chip.textContent)).toEqual([
      "删除",
      "保留",
      "改名 (Recommended)",
    ]);
    expect(screen.getByTestId("home-card-other")).toHaveAttribute("href", "/s/ins_a");
    fireEvent.click(screen.getByRole("button", { name: "保留" }));
    expect(onRespond).toHaveBeenCalledWith(expect.anything(), {
      kind: "question",
      answers: { q0: { optionIds: ["保留"], text: null } },
    });
  });

  it("sends a multi-field question to 去回答 with no answer control", () => {
    renderCard(interaction({ id: "int_q", instanceId: "ins_a", request: question([field(), field({ id: "q1" })]) }));
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.getByTestId("home-card-answer")).toHaveTextContent("去回答");
  });

  it("never offers an approve control on a plan: 审阅计划 only", () => {
    renderCard(interaction({ id: "int_p", instanceId: "ins_a", request: PLAN }));
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.queryByText("批准")).toBeNull();
    expect(screen.getByTestId("home-card-review")).toHaveTextContent("审阅计划");
    // §4.2 标题、正文前 2 行: the title is its own line (outside the body's
    // 2-line budget), and the body carries exactly the first two plan lines —
    // each rendered in full, never clipped into a title+1 budget.
    expect(screen.getByTestId("home-card-plan-title").textContent).toBe("Plan: split the gate");
    const body = screen.getByTestId("home-card-body");
    expect(body.textContent).toBe("1. read the gate\n2. split it");
    expect(body.textContent!.split("\n")).toHaveLength(2);
  });

  it("shows a terminal prompt's last 3 screen lines and 去终端", () => {
    const item = interaction({
      id: "int_t",
      instanceId: "ins_a",
      carrier: "native-tty",
      request: question([field({ input: "text", options: [], description: "Continue?" })]),
    });
    renderCard(item, { screenLines: ["one", "two", "", "three", "Continue? (y/n)"] });
    expect(screen.getByTestId("home-card-body").textContent).toBe("two\nthree\nContinue? (y/n)");
    expect(screen.getByTestId("home-card-tty")).toHaveAttribute("href", "/s/ins_a/tty");
    expect(screen.queryAllByRole("button")).toHaveLength(0);
  });

  it("gives a sensitive question only 去会话", () => {
    renderCard(interaction({ id: "int_s", instanceId: "ins_a", request: question([field({ sensitive: true })]) }));
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(screen.getByTestId("home-card-session")).toHaveTextContent("去会话");
  });

  it("greys the buttons and says 数据可能过期 while stale (§8)", () => {
    renderCard(interaction({ id: "int_a", instanceId: "ins_a" }), { stale: true });
    for (const button of screen.getAllByRole("button")) expect(button).toBeDisabled();
    expect(screen.getByTestId("home-card-stale")).toHaveTextContent("数据可能过期");
  });

  it("keeps the buttons disabled while the answer is in flight (no optimism)", () => {
    renderCard(interaction({ id: "int_a", instanceId: "ins_a" }), {}, "answering");
    for (const button of screen.getAllByRole("button")) expect(button).toBeDisabled();
    expect(screen.getByTestId("home-card-submitting")).toBeInTheDocument();
  });
});

/* ------------------------------------------------------------------ */
/* §6.4 交给 Main: question and plan cards only, never an approval.    */
/* ------------------------------------------------------------------ */

describe('DecisionCard mode="home" 交给 Main (§6.4)', () => {
  it("offers 交给 Main on a QUESTION card, without answering it", async () => {
    const run = vi.fn().mockResolvedValue(undefined);
    const onRespond = renderCard(
      interaction({ id: "int_q", instanceId: "ins_a", request: question() }),
      { handToMain: run },
    );
    fireEvent.click(screen.getByTestId("home-card-more"));
    const item = screen.getByTestId("home-card-hand-main");
    expect(item).toHaveTextContent("交给 Main");
    fireEvent.click(item);
    expect(run).toHaveBeenCalledTimes(1);
    expect(run.mock.calls[0][0]).toEqual(expect.objectContaining({ id: "int_q" }));
    // The interaction itself is untouched: no answer POST, still pending.
    expect(onRespond).not.toHaveBeenCalled();
    // The card stays on the home with its own controls.
    expect(screen.getAllByTestId("home-card-chip").length).toBeGreaterThan(0);
    expect(await screen.findByTestId("home-card-hand-receipt")).toHaveTextContent("已交给 Main");
  });

  it("offers 交给 Main on a PLAN card", () => {
    const run = vi.fn().mockResolvedValue(undefined);
    renderCard(interaction({ id: "int_p", instanceId: "ins_a", kind: "plan-review", request: PLAN }), {
      handToMain: run,
    });
    fireEvent.click(screen.getByTestId("home-card-more"));
    expect(screen.getByTestId("home-card-hand-main")).toHaveTextContent("交给 Main");
  });

  it("never offers it on an approval card, even when a handler exists", () => {
    renderCard(interaction({ id: "int_a", instanceId: "ins_a" }), { handToMain: vi.fn() });
    expect(screen.queryByTestId("home-card-more")).toBeNull();
    expect(screen.queryByTestId("home-card-hand-main")).toBeNull();
  });

  it("hides it when no Main resolves", () => {
    renderCard(interaction({ id: "int_q", instanceId: "ins_a", request: question() }), { handToMain: null });
    expect(screen.queryByTestId("home-card-more")).toBeNull();
  });

  it("greys the trigger while the queue is stale (§8)", () => {
    renderCard(interaction({ id: "int_q", instanceId: "ins_a", request: question() }), {
      handToMain: vi.fn(),
      stale: true,
    });
    expect(screen.getByTestId("home-card-more")).toBeDisabled();
  });
});

