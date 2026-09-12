import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { PtyQuestionAnswers } from "./PtyQuestionAnswers";
import type { Interaction } from "../types/interaction";

function item(freeText = false): Interaction {
  return { request: { kind: "question", title: "Terminal question", fields: [{
    id: "screen", title: "Reply", description: "Which environment?", input: freeText ? "text" : "single-select",
    required: true, sensitive: false, allowFreeText: freeText,
    options: freeText ? [] : [{ id: "2", label: "Staging" }],
  }] } } as Interaction;
}
describe("PTY quick answers", () => {
  it("sends the displayed option and field IDs", () => {
    const answer = vi.fn();
    render(<PtyQuestionAnswers item={item()} disabled={false} onAnswer={answer} />);
    fireEvent.click(screen.getByRole("button", { name: "Staging" }));
    expect(answer).toHaveBeenCalledWith({ kind: "question", answers: { screen: { optionIds: ["2"], text: null } } });
  });
  it("disables offline or unanswerable requests", () => {
    const answer = vi.fn();
    render(<PtyQuestionAnswers item={item()} disabled onAnswer={answer} />);
    fireEvent.click(screen.getByRole("button", { name: "Staging" }));
    expect(answer).not.toHaveBeenCalled();
  });
  it("submits a text question through the broker", () => {
    const answer = vi.fn();
    render(<PtyQuestionAnswers item={item(true)} disabled={false} onAnswer={answer} />);
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "demo" } });
    fireEvent.click(screen.getByRole("button", { name: "发送回答" }));
    expect(answer).toHaveBeenCalledWith({ kind: "question", answers: { screen: { optionIds: [], text: "demo" } } });
  });
});
