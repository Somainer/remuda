import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { CodeBlock } from "./CodeBlock";
import { highlightCode } from "../lib/highlight";
import { listenForCodeQuotes, quoteCode } from "../lib/codeQuoteBus";

beforeAll(async () => {
  await highlightCode("ts", "const a = 1;");
});

afterEach(() => cleanup());

function mountInMessage(ui: React.ReactNode) {
  const { container } = render(
    <div data-testid="transcript">
      <section data-testid="message">
        <div>{ui}</div>
      </section>
    </div>,
  );
  return container;
}

describe("CodeBlock 评论 action", () => {
  it("hides the button without a mounted session composer", () => {
    render(<CodeBlock code="const a = 1;" info="ts" />);
    expect(screen.queryByTestId("code-comment")).toBeNull();
  });

  it("appears when a composer subscribes and quotes the whole block", async () => {
    const received: unknown[] = [];
    const stop = listenForCodeQuotes((quote) => received.push(quote));
    try {
      mountInMessage(<CodeBlock code={"const a = 1;\nconst b = 2;"} info="ts src/a.ts" />);
      const button = await screen.findByTestId("code-comment");
      expect(button).toHaveAttribute("aria-label", "评论：把这段代码引用到输入框");
      expect(button).toHaveAttribute("title", "评论");
      fireEvent.click(button);
      expect(received).toHaveLength(1);
      expect(received[0]).toEqual({
        kind: "code",
        turnOrdinal: 1,
        blockOrdinal: 0,
        lang: "ts",
        path: "src/a.ts",
        text: "const a = 1;\nconst b = 2;",
        lineFrom: 1,
        lineTo: 2,
      });
    } finally {
      stop();
    }
  });

  it("returns null from the bus when no composer is mounted", () => {
    const spy = vi.fn();
    quoteCode({
      kind: "code",
      turnOrdinal: 1,
      blockOrdinal: 0,
      lang: "ts",
      text: "x",
      lineFrom: 1,
      lineTo: 1,
    });
    expect(spy).not.toHaveBeenCalled();
  });

  it("counts the block ordinal within its own message", async () => {
    const received: unknown[] = [];
    const stop = listenForCodeQuotes((quote) => received.push(quote));
    try {
      render(
        <div data-testid="transcript">
          <section data-testid="message">
            <div>
              <CodeBlock code="first" info="ts" />
              <CodeBlock code="second()" info="ts" />
            </div>
          </section>
        </div>,
      );
      const buttons = await screen.findAllByTestId("code-comment");
      fireEvent.click(buttons[1]!);
      expect(received[0]).toMatchObject({ blockOrdinal: 1, text: "second()" });
    } finally {
      stop();
    }
  });

  it("quotes only the selected lines when the viewer selected inside the pre", async () => {
    const received: unknown[] = [];
    const stop = listenForCodeQuotes((quote) => received.push(quote));
    try {
      const code = "line one\nline two\nline three";
      mountInMessage(<CodeBlock code={code} info="ts" />);
      await screen.findByTestId("code-comment");
      // Highlighting replaces the pre's text nodes asynchronously (or leaves
      // plain text when there are no tokens); wait until the DOM settles so a
      // Range built against its text nodes doesn't detach.
      await waitFor(() =>
        expect(screen.getByTestId("code-pre").textContent).toContain("line three"),
      );
      await new Promise((resolve) => setTimeout(resolve, 0));
      const pre = screen.getByTestId("code-pre");
      // Select "line two" across whatever text nodes highlighting produced.
      // jsdom's Selection ignores addRange, so install a stub Selection that
      // reports the range the browser would have.
      const range = makeRangeAcrossText(pre, code, "line two");
      const original = window.getSelection.bind(window);
      window.getSelection = () =>
        ({
          isCollapsed: range.collapsed,
          rangeCount: 1,
          getRangeAt: () => range,
        } as unknown as Selection);

      fireEvent.click(await screen.findByTestId("code-comment"));
      await waitFor(() => expect(received).toHaveLength(1));
      expect(received[0]).toMatchObject({ lineFrom: 2, lineTo: 2, text: "line two" });
      window.getSelection = original;
    } finally {
      stop();
    }
  });
});

/** Build a real DOM Range spanning `needle` across the block's text nodes. */
function makeRangeAcrossText(root: HTMLElement, full: string, needle: string): Range {
  const target = full.indexOf(needle);
  const texts: { node: Text; start: number }[] = [];
  let acc = 0;
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode() as Text | null;
  while (node) {
    texts.push({ node, start: acc });
    acc += node.data.length;
    node = walker.nextNode() as Text | null;
  }
  const at = (offset: number) => {
    for (const entry of texts) {
      if (offset <= entry.start + entry.node.data.length) {
        return { node: entry.node, offsetInNode: offset - entry.start };
      }
    }
    const last = texts[texts.length - 1]!;
    return { node: last.node, offsetInNode: last.node.data.length };
  };
  const range = document.createRange();
  const a = at(target);
  const b = at(target + needle.length);
  range.setStart(a.node, a.offsetInNode);
  range.setEnd(b.node, b.offsetInNode);
  return range;
}
