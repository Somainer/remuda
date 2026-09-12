import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { AuxKeys } from "./AuxKeys";

describe("terminal aux keys", () => {
  it("sends esc/tab/arrows/pgup/ctrl+c and sticky ctrl", async () => {
    const user = userEvent.setup();
    const onKey = vi.fn();
    render(<AuxKeys disabled={false} onKey={onKey} />);
    await user.click(screen.getByTestId("tty-key-esc"));
    await user.click(screen.getByTestId("tty-key-tab"));
    await user.click(screen.getByTestId("tty-key-left"));
    await user.click(screen.getByTestId("tty-key-pgup"));
    await user.click(screen.getByTestId("tty-key-pgdn"));
    await user.click(screen.getByTestId("tty-key-ctrl-c"));
    expect(onKey.mock.calls.map((call) => call[0])).toEqual([
      "\u001b",
      "\t",
      "\u001b[D",
      "\u001b[5~",
      "\u001b[6~",
      "\u0003",
    ]);
  });
});
