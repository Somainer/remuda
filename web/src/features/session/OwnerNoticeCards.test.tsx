import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("../../lib/store", () => ({
  hubStore: {
    subscribeRefreshHealth: () => () => undefined,
    getRefreshHealth: () => ({ lastOkAt: null, failingSince: null }),
  },
}));

import { OwnerNoticeCards } from "./OwnerNoticeCards";
import { T2, notice } from "../home/queueTestData";

/**
 * D-060 §11.5: Main's 「告诉你」 card in the transcript — Main's labelled
 * voice with the whole body, never an owner bubble; 知道了 until the owner
 * acknowledged it, then 已确认 (the card stays in the conversation).
 */
describe("OwnerNoticeCards", () => {
  it("renders Main's labelled card with the whole body and 知道了", async () => {
    const onAck = vi.fn().mockResolvedValue(undefined);
    render(<OwnerNoticeCards notices={[notice({ noticeId: "ntc_1" })]} onAck={onAck} />);
    const card = screen.getByTestId("transcript-notice");
    expect(card).toHaveAttribute("data-acked", "0");
    expect(within(card).getByText("Main 告诉你")).toBeInTheDocument();
    expect(card).toHaveTextContent("Nightly gate red");
    expect(card).toHaveTextContent("I paused landing.");
    // Not a user bubble: no message role, its own labelled article.
    expect(card.tagName).toBe("ARTICLE");
    expect(card).toHaveAttribute("aria-label", "Main 告诉你：Nightly gate red");
    await act(async () => {
      fireEvent.click(within(card).getByTestId("transcript-notice-ack"));
    });
    expect(onAck).toHaveBeenCalledWith("ntc_1");
  });

  it("shows 已确认 once acknowledged and an inline error when the ack fails", async () => {
    const onAck = vi.fn().mockRejectedValue(new Error("hub down"));
    render(
      <OwnerNoticeCards
        notices={[notice({ noticeId: "ntc_done", ackedAt: T2 }), notice({ noticeId: "ntc_open" })]}
        onAck={onAck}
      />,
    );
    const [done, open] = screen.getAllByTestId("transcript-notice");
    expect(within(done).getByTestId("transcript-notice-acked")).toHaveTextContent("已确认");
    expect(within(done).queryByTestId("transcript-notice-ack")).toBeNull();
    await act(async () => {
      fireEvent.click(within(open).getByTestId("transcript-notice-ack"));
    });
    expect(within(open).getByRole("alert")).toHaveTextContent("hub down");
    expect(within(open).getByTestId("transcript-notice-ack")).toBeEnabled();
  });
});
