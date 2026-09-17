import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { QuestionForm } from "./QuestionForm";
import type { Interaction } from "../../types/interaction";

/// A hook-carried AskUserQuestion, shaped exactly like the Node builds it from
/// a real `PermissionRequest` (2.1.272; fixture
/// crates/remuda-signal/fixtures/askuser/permission-request.json).
function hookQuestion(overrides: Partial<Interaction> = {}): Interaction {
  return {
    id: "int_0199a1f0-0000-7000-8000-000000000010",
    instanceId: "ins_0199a1f0-0000-7000-8000-000000000001",
    runId: null,
    hostId: "hst_0199a1f0-0000-7000-8000-000000000002",
    kind: "question",
    requestVersion: "1",
    revision: "1",
    createdAt: "2026-09-16T00:00:00.000Z",
    updatedAt: "2026-09-16T00:00:00.000Z",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "harness-hook",
    request: {
      kind: "question",
      title: "AskUserQuestion",
      fields: [
        {
          id: "q0",
          title: "接下来这一步你想怎么走？",
          description: "下一步",
          input: "single-select",
          required: true,
          options: [
            { id: "继续排查", label: "继续排查", description: "沿着当前线索继续深入。" },
            { id: "回到GravityDB", label: "回到GravityDB", description: "切回 GravityDB。" },
          ],
          allowFreeText: true,
          sensitive: false,
        },
        {
          id: "q1",
          title: "需要把哪些内容保存到记忆里？",
          description: "记忆",
          input: "multi-select",
          required: true,
          options: [
            { id: "保存端口", label: "保存端口", description: "记录端口配置。" },
            { id: "保存环境变量", label: "保存环境变量", description: "记录环境变量。" },
          ],
          allowFreeText: true,
          sensitive: false,
        },
      ],
    },
    deadline: { known: true, value: "2026-09-16T00:15:00.000Z" },
    deadlineSource: "runtime-policy",
    answer: { known: false, reason: "pending", evidenceEventIds: [] },
    delivery: "not-sent",
    resolution: { known: false, reason: "pending", evidenceEventIds: [] },
    ...overrides,
  } as unknown as Interaction;
}

describe("QuestionForm for an AskUserQuestion hook", () => {
  it("renders one tab per question with the header as title", () => {
    render(<QuestionForm interaction={hookQuestion()} onRespond={() => {}} />);
    expect(screen.getByRole("tab", { name: /下一步/ })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /记忆/ })).toBeInTheDocument();
    // Only the current panel's question text is shown.
    expect(screen.getByText("接下来这一步你想怎么走？")).toBeInTheDocument();
    expect(screen.queryByText("需要把哪些内容保存到记忆里？")).toBeNull();
  });

  it("shows labels and descriptions as radio / checkbox rows, never the raw JSON", () => {
    const { container } = render(<QuestionForm interaction={hookQuestion()} onRespond={() => {}} />);
    const radio = screen.getByRole("radio", { name: /继续排查/ });
    expect(radio).toHaveAttribute("aria-checked", "false");
    expect(screen.getByText("沿着当前线索继续深入。")).toBeInTheDocument();
    // Raw questions JSON is hidden behind the disclosure.
    expect(container.querySelector("[data-testid='question-raw']")).toBeNull();
  });

  it("collects a single-select and a multi-select and submits them in one answer", async () => {
    const onRespond = vi.fn();
    render(<QuestionForm interaction={hookQuestion()} onRespond={onRespond} />);
    await userEvent.click(screen.getByRole("radio", { name: /回到GravityDB/ }));
    await userEvent.click(screen.getByRole("tab", { name: /记忆/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /保存端口/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /保存环境变量/ }));
    expect(screen.getByTestId("question-submit")).toBeEnabled();
    await userEvent.click(screen.getByTestId("question-submit"));
    expect(onRespond).toHaveBeenCalledTimes(1);
    expect(onRespond).toHaveBeenCalledWith({
      kind: "question",
      answers: {
        q0: { optionIds: ["回到GravityDB"], text: null },
        q1: { optionIds: ["保存端口", "保存环境变量"], text: null },
      },
    });
  });

  it("sends free text as the field's text answer", async () => {
    const onRespond = vi.fn();
    render(<QuestionForm interaction={hookQuestion()} onRespond={onRespond} />);
    await userEvent.type(screen.getByTestId("question-free-q0"), "先喝杯茶");
    // Typing free text clears any option choice on that field.
    expect(screen.getByRole("radio", { name: /继续排查/ })).toHaveAttribute("aria-checked", "false");
    await userEvent.click(screen.getByRole("tab", { name: /记忆/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /保存端口/ }));
    await userEvent.click(screen.getByTestId("question-submit"));
    expect(onRespond.mock.calls[0][0].answers.q0).toEqual({ optionIds: [], text: "先喝杯茶" });
  });

  it("keeps submit disabled until every required question is answered", async () => {
    render(<QuestionForm interaction={hookQuestion()} onRespond={() => {}} />);
    expect(screen.getByTestId("question-submit")).toBeDisabled();
    await userEvent.click(screen.getByRole("radio", { name: /继续排查/ }));
    // One of two questions is not enough.
    expect(screen.getByTestId("question-submit")).toBeDisabled();
    await userEvent.click(screen.getByRole("tab", { name: /记忆/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /保存端口/ }));
    expect(screen.getByTestId("question-submit")).toBeEnabled();
  });

  it("digits select options and Enter submits", async () => {
    const onRespond = vi.fn();
    render(<QuestionForm interaction={hookQuestion()} onRespond={onRespond} />);
    const form = screen.getByTestId("question-form");
    await userEvent.type(form, "1");
    // Single-select advances to the second question.
    expect(screen.getByText("需要把哪些内容保存到记忆里？")).toBeInTheDocument();
    await userEvent.type(form, "1");
    await userEvent.type(form, "{Enter}");
    expect(onRespond).toHaveBeenCalledWith({
      kind: "question",
      answers: {
        q0: { optionIds: ["继续排查"], text: null },
        q1: { optionIds: ["保存端口"], text: null },
      },
    });
  });

  it("the secondary deny button sends an empty batch answer", async () => {
    const onRespond = vi.fn();
    render(<QuestionForm interaction={hookQuestion()} onRespond={onRespond} />);
    await userEvent.click(screen.getByTestId("question-deny"));
    expect(onRespond).toHaveBeenCalledWith({ kind: "question", answers: {} });
  });

  it("reveals the raw request only behind the 原始 disclosure", async () => {
    render(<QuestionForm interaction={hookQuestion()} onRespond={() => {}} />);
    expect(screen.queryByTestId("question-raw")).toBeNull();
    await userEvent.click(screen.getByTestId("question-raw-toggle"));
    const raw = screen.getByTestId("question-raw");
    expect(raw.textContent).toContain('"kind": "question"');
    expect(raw.textContent).toContain("接下来这一步你想怎么走？");
  });

  it("locks the form while busy or already resolved", () => {
    render(<QuestionForm interaction={hookQuestion({ state: "resolved" })} onRespond={() => {}} />);
    expect(screen.getByTestId("question-submit")).toBeDisabled();
    expect(screen.getByTestId("question-deny")).toBeDisabled();
    for (const radio of screen.getAllByRole("radio")) expect(radio).toBeDisabled();
  });

  it("renders a single-question card without tabs", () => {
    const two = hookQuestion();
    if (two.request.kind !== "question") throw new Error("fixture must be a question");
    const one = hookQuestion({
      request: { kind: "question", title: "AskUserQuestion", fields: [two.request.fields[0]] },
    });
    render(<QuestionForm interaction={one} onRespond={() => {}} />);
    expect(screen.queryByRole("tablist")).toBeNull();
    expect(within(screen.getByTestId("question-form")).getByText("接下来这一步你想怎么走？")).toBeInTheDocument();
  });
});
