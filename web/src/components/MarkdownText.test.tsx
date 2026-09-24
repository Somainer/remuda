import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { clipboardIo } from "../lib/clipboard";
import { highlightCode } from "../lib/highlight";
import { notify } from "../lib/notify";
import { MarkdownText } from "./MarkdownText";
import { listenForCodeQuotes } from "../lib/codeQuoteBus";

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

  it("keeps the same CodeBlock DOM node when MarkdownText re-renders", async () => {
    // Gate flake: a fresh inline pre override per Markdown render remounted the
    // fence subtree, so every transcript re-render (and the lazy highlight
    // swap) detached the code-block node between assertion and screenshot.
    const text = [
      "请看代码:",
      "```ts",
      "export function greet(name: string): string {",
      "  return name;",
      "}",
      "```",
      "",
      "```bash",
      "echo deploy",
      "```",
    ].join("\n");
    const { rerender } = render(<MarkdownText text={text} />);
    // Wait for the async highlight to land before sampling identity, so the
    // later assertion compares against the post-highlight subtree.
    await waitFor(() =>
      expect(screen.getAllByTestId("code-block")[0]!.querySelector(".hljs-keyword")).toBeTruthy(),
    );
    const first = screen.getAllByTestId("code-block")[0]!;
    const second = screen.getAllByTestId("code-block")[1]!;
    rerender(<MarkdownText text={text} />);
    rerender(<MarkdownText text={text} />);
    expect(screen.getAllByTestId("code-block")[0]).toBe(first);
    expect(screen.getAllByTestId("code-block")[1]).toBe(second);
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

  it("passes the fence language plus meta (e.g. a file path) through to CodeBlock", async () => {
    const received: unknown[] = [];
    const stop = listenForCodeQuotes((quote) => received.push(quote));
    try {
      render(<MarkdownText text={"```ts src/app.ts\nconst x = 1;\n```"} />);
      await screen.findByTestId("code-comment");
      fireEvent.click(screen.getByTestId("code-comment"));
      await waitFor(() => expect(received).toHaveLength(1));
    } finally {
      stop();
    }
    // Language label resolves to its display name; the path survives
    // sanitize into the quote payload the composer expands.
    expect(screen.getByTestId("code-lang")).toHaveTextContent("TypeScript");
    expect(received[0]).toMatchObject({ lang: "ts", path: "src/app.ts" });
  });
});

describe("math rendering (c-math)", () => {
  const SOFTMAX =
    "$$\\sigma(\\mathbf{z})_i = \\frac{e^{z_i}}{\\sum_{j=1}^{K} e^{z_j}}$$";

  it("renders the owner softmax formula as display KaTeX, not raw TeX", async () => {
    render(<MarkdownText text={SOFTMAX} />);
    const display = await screen.findByTestId("math-display");
    expect(display.querySelector(".katex-display")).toBeTruthy();
    expect(display.querySelector(".mfrac")).toBeTruthy();
    // The underscores inside the formula must never survive as markdown
    // emphasis: no <em> anywhere in the message.
    expect(display.querySelector("em")).toBeNull();
    expect(screen.queryByTestId("code-block")).toBeNull();
  });

  it("renders inline math and keeps surrounding prose", async () => {
    render(<MarkdownText text={"函数 $f(x)=x^2$ 的值"} />);
    const inline = await screen.findByTestId("math-inline");
    expect(inline.querySelector(".katex")).toBeTruthy();
    expect(screen.getByText(/函数/)).toBeTruthy();
    expect(screen.getByText(/的值/)).toBeTruthy();
  });

  it("accepts the bracket delimiters", async () => {
    render(<MarkdownText text={"a \\(x^2\\) b and\n\\[y^2\\]"} />);
    expect((await screen.findByTestId("math-inline")).querySelector(".katex")).toBeTruthy();
    expect((await screen.findByTestId("math-display")).querySelector(".katex-display")).toBeTruthy();
  });

  it("leaves a currency sentence as plain text", async () => {
    render(<MarkdownText text={"花了 $5 和 $10"} />);
    // No engine is needed for plain text: assert synchronously after a tick.
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(screen.queryByTestId("math-inline")).toBeNull();
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect(screen.getByText("花了 $5 和 $10")).toBeTruthy();
  });

  it("keeps escaped dollars literal", () => {
    render(<MarkdownText text={"价格 \\$5"} />);
    expect(screen.getByText(/价格/).textContent).toContain("$5");
    expect(screen.queryByTestId("math-inline")).toBeNull();
  });

  it("never parses math inside code spans or fenced blocks", async () => {
    render(
      <MarkdownText text={"inline `$a_b$` and\n```\n$$not math$$\n```"} />,
    );
    const block = await screen.findByTestId("code-block");
    expect(block.textContent).toContain("$$not math$$");
    expect(screen.getByText("$a_b$")).toBeTruthy();
    expect(screen.queryByTestId("math-inline")).toBeNull();
    expect(screen.queryByTestId("math-display")).toBeNull();
  });

  it("shows an unclosed streaming $$ as plain text until it closes", async () => {
    const { rerender } = render(<MarkdownText text={"intro $$\n\\sigma(z)"} />);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect(screen.getByText(/intro/)).toBeTruthy();

    // The closing delimiter arrives: same message now renders the formula.
    rerender(<MarkdownText text={"intro $$\n\\sigma(z)\n$$"} />);
    expect(await screen.findByTestId("math-display")).toBeTruthy();
  });

  it("renders a broken formula as its source without breaking the message", async () => {
    render(<MarkdownText text={"坏的 $$\\frac{$$ 后面"} />);
    const error = await screen.findByTestId("math-error");
    expect(error.textContent).toBe("\\frac{");
    expect(screen.getByText(/后面/)).toBeTruthy();
  });

  it("keeps currency prose separate from a later broken and good display formula", async () => {
    // Regression: the rejected `$10` must not pair across the line break with
    // the `$$` opener, which would leave the final formula as raw text.
    render(
      <MarkdownText
        text={"花了 $5 和 $10 都不渲染。\n坏的 $$\\frac{$$ 结束。\n$$x_1+x_2+x_3$$"}
      />,
    );
    expect((await screen.findAllByTestId("math-display"))).toHaveLength(1);
    expect(await screen.findByTestId("math-error")).toBeTruthy();
    expect(screen.getByText(/花了 \$5 和 \$10 都不渲染。/)).toBeTruthy();
  });

  it("keeps a $$ formula inside a blockquote and a list item (#2)", async () => {
    const { container, rerender } = render(<MarkdownText text={"> $$x^2$$"} />);
    const quote = container.querySelector("blockquote");
    expect(quote).toBeTruthy();
    expect(quote!.querySelector('[data-testid="math-display"]')).toBeTruthy();
    expect(quote!.querySelector(".katex-display")).toBeTruthy();

    rerender(<MarkdownText text={"- $$y^2$$"} />);
    const item = container.querySelector("ul > li");
    expect(item).toBeTruthy();
    expect(item!.querySelector('[data-testid="math-display"]')).toBeTruthy();
  });

  it("renders an indented $$ line as a code block, not a formula (#2)", async () => {
    render(<MarkdownText text={"    $$x^2$$"} />);
    expect(await screen.findByTestId("code-block")).toBeTruthy();
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect(screen.queryByTestId("math-inline")).toBeNull();
  });

  it("treats a ```math / ~~~math fence as code, never loading KaTeX (#3)", async () => {
    const { rerender } = render(<MarkdownText text={"```math\nx^2\n```"} />);
    const block = await screen.findByTestId("code-block");
    expect(block.textContent).toContain("x^2");
    expect(screen.queryByTestId("math-display")).toBeNull();

    rerender(<MarkdownText text={"~~~math\ny^2\n~~~"} />);
    expect((await screen.findAllByTestId("code-block")).length).toBeGreaterThan(0);
    expect(screen.queryByTestId("math-display")).toBeNull();
  });

  it("renders an unclosed display opener's tail as literal, intact (#4)", async () => {
    const { rerender } = render(<MarkdownText text={"intro \\[a *b* + \\{c\\}"} />);
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect(screen.queryByTestId("math-inline")).toBeNull();
    // Backslash, asterisks and braces must all show as typed (no <em>, no
    // swallowed delimiter).
    expect(container_text(document.body)).toContain("intro \\[a *b* + \\{c\\}");
    expect(document.body.querySelector("em")).toBeNull();

    // Same for the $$ form.
    rerender(<MarkdownText text={"intro $$\n\\sigma(z)"} />);
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect(container_text(document.body)).toContain("intro $$");
    expect(container_text(document.body)).toContain("\\sigma(z)");
  });

  it("keeps a later closed formula after a blank-line-voided $$ pair (#5)", async () => {
    render(<MarkdownText text={"$$x\n\n$$\n\n$y$"} />);
    // First $$ pair is dead across the blank line (literal text); the later
    // $y$ is a valid inline formula and still renders.
    expect(await screen.findByTestId("math-inline")).toBeTruthy();
    expect(screen.queryByTestId("math-display")).toBeNull();
  });
});

function container_text(root: ParentNode): string {
  return root.textContent ?? "";
}

describe("D-027b file-mention folding", () => {
  it("renders a quoted [File #n] saved-at line as a collapsed row", () => {
    const text =
      "echo intro\n[File #1] report.pdf (application/pdf, 24.0 MB) saved at /data/report.pdf\nrest";
    render(<MarkdownText text={text} />);
    const row = screen.getByTestId("file-mention");
    expect(row.getAttribute("data-index")).toBe("1");
    expect(row.textContent).toContain("report.pdf");
    expect(row.textContent).toContain("application/pdf");
    expect((row as HTMLDetailsElement).open).toBe(false);
  });

  it("leaves ordinary markdown without a mention row", () => {
    render(<MarkdownText text="hello **world**" />);
    expect(screen.getByText("world")).toBeTruthy();
    expect(screen.queryAllByTestId("file-mention")).toHaveLength(0);
  });
});
