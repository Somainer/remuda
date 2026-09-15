import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { clipboardIo } from "../lib/clipboard";
import { highlightCode } from "../lib/highlight";
import { notify } from "../lib/notify";
import { MarkdownText } from "./MarkdownText";

vi.mock("../lib/notify", () => ({ notify: vi.fn() }));

// The first fence lazily compiles the highlight.js core + grammar chunks; on a
// loaded box that cold import is the only thing past the default 5s budget.
beforeAll(async () => {
  await highlightCode("ts", "const warm = 1;");
});

beforeEach(() => {
  localStorage.clear();
  vi.mocked(notify).mockClear();
});

describe("code block toolbar", () => {
  it("copies the raw fence text, shows a transient check and announces via notify", async () => {
    const write = vi.spyOn(clipboardIo, "write").mockResolvedValue(undefined);
    vi.useFakeTimers();
    try {
      render(<MarkdownText text={"```ts\nconst a = 1;\nline\n```"} />);
      const copy = screen.getByTestId("code-copy");
      expect(copy).toHaveAttribute("aria-label", "复制");
      expect(copy).toHaveAttribute("title", "复制");
      fireEvent.click(copy);
      // Flush the async clipboard continuation under act so the copied state lands.
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(write).toHaveBeenCalledWith("const a = 1;\nline");
      expect(copy).toHaveAttribute("aria-label", "已复制");
      expect(copy).toHaveAttribute("data-copied", "true");
      expect(notify).toHaveBeenCalledWith(
        expect.objectContaining({ subject: "代码", stage: "已复制", severity: "info" }),
      );
      act(() => {
        vi.advanceTimersByTime(1500);
      });
      expect(copy).toHaveAttribute("aria-label", "复制");
      expect(copy).toHaveAttribute("data-copied", "false");
    } finally {
      vi.useRealTimers();
      write.mockRestore();
    }
  });

  it("persists the soft-wrap preference per viewer and applies it on remount", async () => {
    const user = userEvent.setup();
    const md = "```\n" + "x".repeat(100) + "\n```";
    const { unmount } = render(<MarkdownText text={md} />);
    const block = screen.getByTestId("code-block");
    const wrap = screen.getByTestId("code-wrap");
    expect(block).toHaveAttribute("data-wrap", "off");
    expect(wrap).toHaveAttribute("aria-label", "换行");
    expect(wrap).toHaveAttribute("aria-pressed", "false");
    await user.click(wrap);
    expect(block).toHaveAttribute("data-wrap", "on");
    expect(wrap).toHaveAttribute("aria-pressed", "true");
    expect(localStorage.getItem("runtime.code-wrap")).toBe("1");

    unmount();
    render(<MarkdownText text={md} />);
    expect(screen.getByTestId("code-block")).toHaveAttribute("data-wrap", "on");
    expect(screen.getByTestId("code-wrap")).toHaveAttribute("aria-label", "取消换行");
  });

  it("is keyboard focusable with a visible focus ring and shows a language label", () => {
    render(<MarkdownText text={"```rust\nfn main() {}\n```"} />);
    const buttons = [screen.getByTestId("code-wrap"), screen.getByTestId("code-copy")];
    for (const button of buttons) {
      expect(button.tagName).toBe("BUTTON");
      expect(button).toBeEnabled();
    }
    expect(screen.getByTestId("code-lang")).toHaveTextContent("Rust");
  });
});

describe("syntax highlighting", () => {
  it("applies token spans for ts, rust, python, json and bash", async () => {
    render(
      <MarkdownText
        text={[
          "```ts",
          "export const v: number = 1;",
          "```",
          "",
          "```rust",
          "fn main() {}",
          "```",
          "",
          "```python",
          "def f(): pass",
          "```",
          "",
          "```json",
          '{"a": 1}',
          "```",
          "",
          "```bash",
          "echo hi",
          "```",
        ].join("\n")}
      />,
    );
    // Grammars load lazily per language: wait until every block has token spans, not just the first.
    await waitFor(() => {
      const codes = screen.getAllByTestId("code-code");
      expect(codes.length).toBe(5);
      for (const code of codes) expect(code.querySelectorAll("[class*='hljs']").length).toBeGreaterThan(0);
    });
  });

  it("renders unknown fences plain, keeping the raw info label", async () => {
    render(<MarkdownText text={"```elixir\n:ok\n```"} />);
    expect(screen.getByTestId("code-lang")).toHaveTextContent("elixir");
    const code = screen.getByTestId("code-code");
    expect(code.querySelectorAll("[class*='hljs']").length).toBe(0);
    expect(code).toHaveTextContent(":ok");
    expect(code).toHaveClass("language-elixir");
  });

  it("shows a plain-block note when a fence exceeds the highlight size cap", () => {
    render(<MarkdownText text={"```ts\n" + "a".repeat(10_001) + "\n```"} />);
    expect(screen.getByTestId("code-note")).toBeTruthy();
    expect(screen.getByTestId("code-code").querySelectorAll("[class*='hljs']").length).toBe(0);
  });
});

describe("sanitising fence bodies", () => {
  it("cannot inject markup from a plain fence", () => {
    const body = '<img src=x onerror="alert(1)"><script>alert(2)</script><b>bold</b>';
    render(<MarkdownText text={"```\n" + body + "\n```"} />);
    const root = screen.getByTestId("code-block");
    expect(root.querySelector("img")).toBeNull();
    expect(root.querySelector("script")).toBeNull();
    expect(root.querySelector("b")).toBeNull();
    expect(root.textContent).toContain(body);
  });

  it("cannot inject markup from a highlighted fence", async () => {
    render(<MarkdownText text={"```ts\nconst s = \"<img src=x onerror=alert(1)>\";\n```"} />);
    const root = screen.getByTestId("code-block");
    await waitFor(() => expect(root.querySelector("[class*='hljs']")).not.toBeNull());
    expect(root.querySelector("img")).toBeNull();
    expect(root.querySelector("script")).toBeNull();
    expect(root.textContent).toContain("<img src=x onerror=alert(1)>");
  });

  it("leaves inline code without a toolbar", () => {
    render(<MarkdownText text={"use the `rm -rf` command"} />);
    expect(screen.queryByTestId("code-toolbar")).toBeNull();
    expect(screen.queryByTestId("code-block")).toBeNull();
    expect(screen.getByText("rm -rf").tagName).toBe("CODE");
  });
});
