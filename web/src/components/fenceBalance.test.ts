// @vitest-environment node
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { describe, expect, it } from "vitest";
import { balanceFences } from "./fenceBalance";

describe("balanceFences", () => {
  it("leaves balanced text untouched", () => {
    const text = "a\n```ts\nconst x = 1;\n```\nb";
    expect(balanceFences(text)).toBe(text);
    expect(balanceFences("plain prose")).toBe("plain prose");
  });

  it("closes an odd fence at the end", () => {
    expect(balanceFences("intro\n```ts\nconst x")).toBe("intro\n```ts\nconst x\n```");
    expect(balanceFences("```\n")).toBe("```\n```");
  });

  it("matches the opener's character and length", () => {
    expect(balanceFences("````md\n```\ninner")).toBe("````md\n```\ninner\n````");
    expect(balanceFences("~~~\ncode")).toBe("~~~\ncode\n~~~");
    // A backtick run does not close a tilde fence.
    expect(balanceFences("~~~\n```\n")).toBe("~~~\n```\n~~~");
  });

  it("ignores inline backticks and over-indented runs", () => {
    expect(balanceFences("use ```x``` inline")).toBe("use ```x``` inline");
    expect(balanceFences("    ```\nindented code")).toBe("    ```\nindented code");
    expect(balanceFences("```a`b\nnot a fence")).toBe("```a`b\nnot a fence");
  });

  it("does not treat a fence with an info string as a closer", () => {
    expect(balanceFences("```\nx\n```js\n")).toBe("```\nx\n```js\n```");
  });
});

/**
 * The code blocks the shipped markdown parser produces, each described by the
 * containers it sits in (li / blockquote nesting depth).
 */
function shape(text: string): string[] {
  const html = renderToStaticMarkup(createElement(Markdown, { remarkPlugins: [remarkGfm] }, text));
  const blocks: string[] = [];
  let at = html.indexOf("<pre");
  while (at >= 0) {
    const before = html.slice(0, at);
    const depth = (tag: string) => before.split(`<${tag}`).length - before.split(`</${tag}>`).length;
    blocks.push(`li${depth("li")}/quote${depth("blockquote")}`);
    at = html.indexOf("<pre", at + 1);
  }
  return blocks;
}

/**
 * Every streaming cut inside the fence body renders the same block structure
 * as the complete text: no extra empty code block appears and vanishes.
 */
function expectStableWhileStreaming(complete: string, bodyStart: string, bodyEnd: string): void {
  expect(balanceFences(complete)).toBe(complete);
  const want = shape(complete);
  const from = complete.indexOf(bodyStart);
  const to = complete.indexOf(bodyEnd) + bodyEnd.length;
  for (let cut = from; cut <= to; cut += 1) {
    expect(shape(balanceFences(complete.slice(0, cut))), `cut ${cut}`).toEqual(want);
  }
}

describe("balanceFences · containers", () => {
  it("keeps a list-contained fence one block while streaming", () => {
    const complete = "- step one\n- ```ts\n  const a = 1;\n  const b = 2;\n  ```\n- step three";
    expect(balanceFences("- ```ts\n  const a")).toBe("- ```ts\n  const a\n  ```");
    expectStableWhileStreaming(complete, "const a", "const b = 2;");
  });

  it("keeps a fence under a list item's paragraph inside the item", () => {
    const complete = "1. run this:\n\n   ```sh\n   pnpm test\n   ```\n2. done";
    expectStableWhileStreaming(complete, "pnpm", "pnpm test");
  });

  it("keeps a blockquote fence one block while streaming", () => {
    const complete = "> quoted:\n> ```\n> inside\n> more\n> ```\nafter";
    expect(balanceFences("> ```\n> inside")).toBe("> ```\n> inside\n> ```");
    expectStableWhileStreaming(complete, "inside", "more");
  });

  it("keeps a tilde fence one block while streaming", () => {
    const complete = "text\n\n~~~py\nprint(1)\nprint(2)\n~~~\n\nend";
    expectStableWhileStreaming(complete, "print(1)", "print(2)");
  });

  it("closes a fence whose container ended", () => {
    // The list item ends at the unindented line, and its fence with it.
    const text = "- ```\n  code\nparagraph";
    expect(balanceFences(text)).toBe(text);
  });

  it("returns already balanced text unchanged, including nested containers", () => {
    for (const text of [
      "- ```ts\n  x\n  ```",
      "> - ```\n>   y\n>   ```",
      "1. a\n   ~~~\n   b\n   ~~~",
      "---\n- - -\n```\nz\n```",
    ]) {
      expect(balanceFences(text)).toBe(text);
    }
  });
});
