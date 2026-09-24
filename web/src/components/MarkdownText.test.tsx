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

describe("math rendering (c-math round 3)", () => {
  const SOFTMAX =
    "$$\\sigma(\\mathbf{z})_i = \\frac{e^{z_i}}{\\sum_{j=1}^{K} e^{z_j}}$$";

  it("renders the owner softmax formula as display KaTeX, not raw TeX", async () => {
    render(<MarkdownText text={SOFTMAX} />);
    const display = await screen.findByTestId("math-display");
    expect(display.querySelector(".katex-display")).toBeTruthy();
    expect(display.querySelector(".mfrac")).toBeTruthy();
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

  it("translates bracket delimiters in place (no structure moved)", async () => {
    render(<MarkdownText text={"a \\(x^2\\) b and \\[y^2\\]"} />);
    expect((await screen.findByTestId("math-inline")).querySelector(".katex")).toBeTruthy();
    expect((await screen.findByTestId("math-display")).querySelector(".katex-display")).toBeTruthy();
  });

  it("renders currency as text but an explicit pair in the same sentence (D)", async () => {
    render(<MarkdownText text={"Cost $5 and $10; use $x$."} />);
    expect(await screen.findByTestId("math-inline")).toBeTruthy();
    expect(screen.getByText(/Cost \$5 and \$10; use/)).toBeTruthy();
  });

  it("leaves shell variables as plain text", async () => {
    render(<MarkdownText text={"echo $HOME and $PATH"} />);
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByTestId("math-inline")).toBeNull();
    expect(screen.getByText("echo $HOME and $PATH")).toBeTruthy();
  });

  it("keeps escaped dollars literal", () => {
    render(<MarkdownText text={"价格 \\$5"} />);
    expect(screen.getByText(/价格/).textContent).toContain("$5");
  });

  it("never parses math inside code spans, fences or indented tilde fences (A)", async () => {
    render(<MarkdownText text={"inline `$a_b$` and\n```\n$$not math$$\n```"} />);
    const block = await screen.findByTestId("code-block");
    expect(block.textContent).toContain("$$not math$$");
    expect(screen.getByText("$a_b$")).toBeTruthy();
    expect(screen.queryByTestId("math-display")).toBeNull();

    const { unmount } = render(
      <MarkdownText text={"  ~~~\n$x$\n  ~~~\nend"} />,
    );
    expect(screen.getAllByTestId("code-block").length).toBeGreaterThan(0);
    unmount();
  });

  it("keeps a same-line $$ inside a blockquote and a list item, as a block (C)", async () => {
    const { container, rerender } = render(<MarkdownText text={"> before $$x^2$$ after"} />);
    const quote = container.querySelector("blockquote")!;
    expect(quote).toBeTruthy();
    expect(screen.getByText(/before/)).toBeTruthy();
    expect(screen.getByText(/after/)).toBeTruthy();
    expect(quote.querySelector('[data-testid="math-display"] .katex-display')).toBeTruthy();

    rerender(<MarkdownText text={"- before $$y^2$$ after"} />);
    const item = container.querySelector("ul > li")!;
    expect(item).toBeTruthy();
    expect(item.textContent).toContain("before");
    expect(item.textContent).toContain("after");
    expect(item.querySelector('[data-testid="math-display"]')).toBeTruthy();
  });

  it("translates a list bracket display in place (C: - \\[x\\])", async () => {
    const { container } = render(<MarkdownText text={"- \\[x^2\\]"} />);
    const item = container.querySelector("ul > li")!;
    expect(item).toBeTruthy();
    expect(item.querySelector('[data-testid="math-display"] .katex-display')).toBeTruthy();
  });

  it("renders an indented $$ line as a code block, not a formula (C)", async () => {
    render(<MarkdownText text={"    $$x^2$$"} />);
    expect(await screen.findByTestId("code-block")).toBeTruthy();
    expect(screen.queryByTestId("math-display")).toBeNull();
  });

  it("treats mathdisplay / mathinline / ~~~math fences as code (F)", async () => {
    const { rerender } = render(<MarkdownText text={"```mathdisplay\nx\n```"} />);
    expect((await screen.findByTestId("code-block")).textContent).toContain("x");
    expect(screen.queryByTestId("math-display")).toBeNull();
    rerender(<MarkdownText text={"```mathinline\ny\n```"} />);
    expect((await screen.findAllByTestId("code-block")).length).toBeGreaterThan(0);
    expect(screen.queryByTestId("math-inline")).toBeNull();
    rerender(<MarkdownText text={"~~~math\nz\n~~~"} />);
    expect((await screen.findAllByTestId("code-block")).length).toBeGreaterThan(0);
  });

  it("renders an unclosed display tail as an exact-source React text node (G)", async () => {
    const { rerender } = render(<MarkdownText text={"intro \\[a *b* + \\{c\\}"} />);
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect(document.body.querySelector("em")).toBeNull();
    const literal = await screen.findByTestId("math-literal");
    expect(literal.textContent).toBe("\\[a *b* + \\{c\\}");

    rerender(<MarkdownText text={"intro $$\n\\sigma(z)"} />);
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByTestId("math-display")).toBeNull();
    const lit2 = await screen.findByTestId("math-literal");
    expect(lit2.textContent).toBe("$$\n\\sigma(z)");
  });

  it("kills only the broken $$ opener, then later $$ and $ render (E)", async () => {
    render(<MarkdownText text={"$$\n\nx\n\n$$y$$\n\n$z$"} />);
    expect(await screen.findByTestId("math-display")).toBeTruthy();
    expect(await screen.findByTestId("math-inline")).toBeTruthy();
  });

  it("renders a broken formula as its source without breaking the message", async () => {
    render(<MarkdownText text={"坏的 $$\\frac{$$ 后面"} />);
    const error = await screen.findByTestId("math-error");
    expect(error.textContent).toBe("\\frac{");
    expect(screen.getByText(/后面/)).toBeTruthy();
  });

  it("closes a streaming formula when the closer arrives", async () => {
    const { rerender } = render(<MarkdownText text={"$$\n\\sigma(z)"} />);
    expect(screen.queryByTestId("math-display")).toBeNull();
    expect((await screen.findByTestId("math-literal")).textContent).toBe("$$\n\\sigma(z)");
    rerender(<MarkdownText text={"$$\n\\sigma(z)\n$$"} />);
    expect(await screen.findByTestId("math-display")).toBeTruthy();
    expect(screen.queryByTestId("math-literal")).toBeNull();
  });
});

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
