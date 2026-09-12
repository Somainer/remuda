import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { clipboardIo } from "../lib/clipboard";
import { MarkdownText } from "./MarkdownText";

describe("MarkdownText code block", () => {
  it("copies fenced text and expands long blocks", async () => {
    const write = vi.spyOn(clipboardIo, "write").mockResolvedValue(undefined);
    const user = userEvent.setup();
    const long = Array.from({ length: 12 }, (_, i) => `line_${i}`).join("\n");
    render(<MarkdownText text={"```js\n" + long + "\n```"} />);
    expect(screen.getByTestId("code-expand")).toHaveTextContent("展开");
    expect(screen.getByTestId("code-pre")).toHaveStyle({ maxHeight: "160px" });
    fireEvent.click(screen.getByTestId("code-copy"));
    expect(write).toHaveBeenCalled();
    const copied = String(write.mock.calls[0]?.[0] ?? "");
    expect(copied).toContain("line_0");
    expect(copied).toContain("line_11");
    await user.click(screen.getByTestId("code-expand"));
    expect(screen.getByTestId("code-expand")).toHaveTextContent("收起");
    expect(screen.getByTestId("code-pre")).not.toHaveStyle({ maxHeight: "160px" });
    write.mockRestore();
  });

  it("does not show expand on short fences", () => {
    render(<MarkdownText text={"```\nshort\n```"} />);
    expect(screen.queryByTestId("code-expand")).toBeNull();
    expect(screen.getByTestId("code-copy")).toBeTruthy();
  });
});
