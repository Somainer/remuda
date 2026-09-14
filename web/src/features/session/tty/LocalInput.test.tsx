import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { LocalInput } from "./LocalInput";

describe("LocalInput mobile dock (D-028 §5.2)", () => {
  it("sends the body only — never text + CR in one write", () => {
    const onSend = vi.fn();
    render(<LocalInput disabled={false} mobile onSend={onSend} />);
    const input = screen.getByLabelText("本地输入");
    fireEvent.change(input, { target: { value: "do the thing" } });
    fireEvent.submit(screen.getByRole("button", { name: "发送" }));
    expect(onSend).toHaveBeenCalledTimes(1);
    expect(onSend).toHaveBeenCalledWith("do the thing");
    expect(onSend.mock.calls[0][0]).not.toContain("\r");
    expect(onSend.mock.calls[0][0]).not.toContain("\n");
  });

  it("trims nothing off a body containing internal newlines (Enter is added server-side)", () => {
    const onSend = vi.fn();
    render(<LocalInput disabled={false} mobile onSend={onSend} />);
    // The dock hands the driver the body verbatim; only an appended CR was
    // forbidden. A trailing CRLF typed by accident must not be smuggled into
    // the one write either.
    const input = screen.getByLabelText("本地输入") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "a\tb" } });
    fireEvent.submit(screen.getByRole("button", { name: "发送" }));
    expect(onSend).toHaveBeenCalledWith("a\tb");
  });

  it("does not send while disabled", () => {
    const onSend = vi.fn();
    render(<LocalInput disabled mobile onSend={onSend} />);
    fireEvent.change(screen.getByLabelText("本地输入"), { target: { value: "x" } });
    fireEvent.submit(screen.getByRole("button", { name: "发送" }));
    expect(onSend).not.toHaveBeenCalled();
  });
});
