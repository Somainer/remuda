// @vitest-environment node
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
