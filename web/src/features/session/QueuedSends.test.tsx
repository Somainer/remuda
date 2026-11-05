import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { QueuedCommand } from "../../lib/api";
import { HubHttpError } from "../../lib/httpError";
import { QueuedSends } from "./QueuedSends";
import { queuedText } from "./queuedText";

/**
 * cb-command-queue 「排队中」: the caller's own undelivered sends, each with
 * 编辑 (same commandId, revision CAS) and 撤回 (never delivered).
 */
function row(over: Partial<QueuedCommand> = {}): QueuedCommand {
  return {
    commandId: "cmd_q1",
    instanceId: "ins_q",
    hostId: "hst_q",
    operation: "instance.send",
    state: "queued",
    resolution: "clear",
    forwarded: false,
    idempotencyKey: null,
    revision: 0,
    held: "hub",
    payload: { input: { type: "prompt", blocks: [{ type: "text", text: "run the gate" }] } },
    createdAt: "2026-11-05T01:00:00.000Z",
    updatedAt: "2026-11-05T01:00:00.000Z",
    ...over,
  };
}

describe("QueuedSends", () => {
  it("renders nothing when nothing is queued", () => {
    const { container } = render(<QueuedSends rows={[]} onEdit={vi.fn()} onWithdraw={vi.fn()} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("lists each queued send with where it waits", () => {
    render(
      <QueuedSends
        rows={[row(), row({ commandId: "cmd_q2", held: "node", state: "accepted", forwarded: true })]}
        onEdit={vi.fn()}
        onWithdraw={vi.fn()}
      />,
    );
    expect(screen.getByRole("region", { name: "排队中" })).toHaveTextContent("排队中 · 2");
    const rows = screen.getAllByTestId("queued-send");
    expect(rows[0]).toHaveTextContent("run the gate");
    expect(rows[0]).toHaveTextContent("主机离线，等待送达");
    expect(rows[1]).toHaveAttribute("data-held", "node");
    expect(rows[1]).toHaveTextContent("等待本回合结束");
  });

  it("edits the text in place and saves it", async () => {
    const onEdit = vi.fn().mockResolvedValue(undefined);
    const user = userEvent.setup();
    render(<QueuedSends rows={[row()]} onEdit={onEdit} onWithdraw={vi.fn()} />);
    await user.click(screen.getByRole("button", { name: "编辑" }));
    const input = screen.getByRole("textbox", { name: "编辑排队中的消息" });
    expect(input).toHaveValue("run the gate");
    expect(screen.getByRole("button", { name: "保存" })).toBeDisabled();
    await user.clear(input);
    await user.type(input, "run the gate, then report");
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(onEdit).toHaveBeenCalledWith(expect.objectContaining({ commandId: "cmd_q1" }), "run the gate, then report");
    await waitFor(() => expect(screen.queryByTestId("queued-send-editor")).toBeNull());
  });

  it("says plainly when the send went out meanwhile", async () => {
    const onEdit = vi
      .fn()
      .mockRejectedValue(new HubHttpError(409, "COMMAND_NOT_QUEUED", "already delivered"));
    const user = userEvent.setup();
    render(<QueuedSends rows={[row()]} onEdit={onEdit} onWithdraw={vi.fn()} />);
    await user.click(screen.getByRole("button", { name: "编辑" }));
    await user.type(screen.getByRole("textbox", { name: "编辑排队中的消息" }), "!");
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("已送达，无法再修改");
  });

  it("withdraws a send", async () => {
    const onWithdraw = vi.fn().mockResolvedValue(undefined);
    const user = userEvent.setup();
    render(<QueuedSends rows={[row()]} onEdit={vi.fn()} onWithdraw={onWithdraw} />);
    await user.click(screen.getByRole("button", { name: "撤回" }));
    expect(onWithdraw).toHaveBeenCalledWith(expect.objectContaining({ commandId: "cmd_q1" }));
  });

  it("reads the prompt text in every send shape", () => {
    expect(queuedText({ input: { text: "a" } })).toBe("a");
    expect(queuedText({ input: "b" })).toBe("b");
    expect(queuedText({ input: { blocks: [{ type: "image" }, { type: "text", text: "c" }] } })).toBe("c");
    expect(queuedText({ text: "d" })).toBe("d");
    expect(queuedText({ prompt: "e" })).toBe("e");
    expect(queuedText(undefined)).toBe("");
  });
});
