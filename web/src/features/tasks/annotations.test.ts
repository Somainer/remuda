import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  addAnnotation,
  anchorMark,
  anchorNumber,
  annotationCount,
  clearAnnotations,
  createAnchorDraft,
  createCardDraft,
  normalizeQuote,
  QUOTE_LIMIT,
  readAnnotations,
  readAnnotationSelection,
  removeAnnotation,
  serializeAnnotationPrefix,
  subscribeAnnotations,
  withAnnotationPrefix,
} from "./annotations";

/**
 * Plan task-model task 9 (t-annotations): anchor creation, badge count, send
 * serialisation and clearing of device-local annotation drafts.
 */

const iid = "ins_ann_test";

beforeEach(() => {
  localStorage.clear();
});
afterEach(() => {
  localStorage.clear();
});

describe("draft creation", () => {
  it("creates a card draft pinned to the task", () => {
    const draft = createCardDraft(" check this edge ", {
      taskId: "tsk_1",
      taskTitle: "SE-02 fix flake",
      now: 1000,
      id: "ann_card_1",
    });
    expect(draft).toMatchObject({
      id: "ann_card_1",
      carrier: "card",
      body: "check this edge",
      taskId: "tsk_1",
      taskTitle: "SE-02 fix flake",
      createdAt: 1000,
    });
  });

  it("creates an anchor draft on a transcript quote", () => {
    const draft = createAnchorDraft(
      { surface: "transcript", messageId: "msg_9", quote: "return a + b;" },
      "this is wrong for strings",
      { now: 2000, id: "ann_anchor_1" },
    );
    expect(draft.carrier).toBe("anchor");
    expect(draft.anchor).toEqual({
      surface: "transcript",
      messageId: "msg_9",
      quote: "return a + b;",
    });
    expect(draft.body).toBe("this is wrong for strings");
  });

  it("normalises whitespace and bounds the quote", () => {
    expect(normalizeQuote("  a\n\n b\t c ")).toBe("a b c");
    const long = "x".repeat(QUOTE_LIMIT + 50);
    const bounded = normalizeQuote(long);
    expect(bounded.length).toBe(QUOTE_LIMIT);
    expect(bounded.endsWith("…")).toBe(true);
  });

  it("numbers anchor drafts ① in creation order, card drafts never consume a mark", () => {
    const card = createCardDraft("card note", { now: 1, id: "c1" });
    const a1 = createAnchorDraft({ surface: "transcript", quote: "one" }, "b1", { now: 2, id: "a1" });
    const a2 = createAnchorDraft({ surface: "task-detail", quote: "two" }, "b2", { now: 3, id: "a2" });
    const drafts = [card, a1, a2];
    expect(anchorNumber(drafts, "a1")).toBe(1);
    expect(anchorNumber(drafts, "a2")).toBe(2);
    expect(anchorMark(1)).toBe("①");
    expect(anchorMark(20)).toBe("⑳");
    expect(anchorMark(21)).toBe("(21)");
  });
});

describe("badge count", () => {
  it("counts every non-blank draft the next send carries", () => {
    expect(annotationCount([])).toBe(0);
    const drafts = [
      createCardDraft("one", { now: 1, id: "c" }),
      createAnchorDraft({ surface: "transcript", quote: "q" }, "two", { now: 2, id: "a" }),
    ];
    expect(annotationCount(drafts)).toBe(2);
  });
});

describe("send serialisation", () => {
  const drafts = [
    createCardDraft("rerun with the seed fixed", {
      taskId: "tsk_1",
      taskTitle: "SE-02 flake",
      now: 1,
      id: "c1",
    }),
    createAnchorDraft(
      { surface: "task-detail", messageId: null, quote: "mandate says ship friday" },
      "that date slipped",
      { now: 2, id: "a1" },
    ),
  ];

  it("prefixes a structured block with both carriers", () => {
    const prefix = serializeAnnotationPrefix(drafts);
    expect(prefix).toBe(
      [
        "【批注 ×2】",
        "1. 卡片批注（SE-02 flake）：rerun with the seed fixed",
        "2. 文本标记 · 任务详情「mandate says ship friday」：that date slipped",
      ].join("\n"),
    );
    // Feedback wording only: no protocol verbs, no new wire field name.
    expect(prefix).not.toMatch(/protocol|board_column|set_task_state/i);
  });

  it("folds the block in front of the prompt and is byte-identical with none", () => {
    expect(withAnnotationPrefix([], "hello")).toBe("hello");
    expect(withAnnotationPrefix(drafts, "please act")).toBe(
      `${serializeAnnotationPrefix(drafts)}\n\nplease act`,
    );
    // An attachment-only send (empty prompt) still carries the block.
    expect(withAnnotationPrefix(drafts, "")).toBe(serializeAnnotationPrefix(drafts));
  });

  it("ignores blank-bodied drafts", () => {
    const blank = createCardDraft("   ", { now: 9, id: "blank" });
    expect(withAnnotationPrefix([blank], "go")).toBe("go");
  });

  it("labels transcript anchors as 会话记录", () => {
    const draft = createAnchorDraft(
      { surface: "transcript", messageId: "m1", quote: "export function add" },
      "also handle strings",
      { now: 1, id: "a" },
    );
    expect(serializeAnnotationPrefix([draft])).toContain("文本标记 · 会话记录");
  });
});

describe("device-local storage and clearing", () => {
  it("adds, lists and removes per instance", () => {
    addAnnotation(iid, createCardDraft("note a", { now: 1, id: "n1" }));
    addAnnotation(iid, createAnchorDraft({ surface: "transcript", quote: "q" }, "note b", { now: 2, id: "n2" }));
    expect(readAnnotations(iid).map((d) => d.id)).toEqual(["n1", "n2"]);

    addAnnotation("ins_other", createCardDraft("other session", { now: 3, id: "n3" }));
    expect(readAnnotations("ins_other").map((d) => d.id)).toEqual(["n3"]);
    expect(readAnnotations(iid)).toHaveLength(2);

    removeAnnotation(iid, "n1");
    expect(readAnnotations(iid).map((d) => d.id)).toEqual(["n2"]);
  });

  it("keeps drafts ordered by creation time and drops blank bodies", () => {
    addAnnotation(iid, createCardDraft("later", { now: 20, id: "late" }));
    addAnnotation(iid, createCardDraft("earlier", { now: 10, id: "early" }));
    addAnnotation(iid, createCardDraft("  ", { now: 30, id: "blank" }));
    expect(readAnnotations(iid).map((d) => d.id)).toEqual(["early", "late"]);
  });

  it("clears after a send and removes the storage key", () => {
    addAnnotation(iid, createCardDraft("rides the send", { now: 1, id: "n1" }));
    expect(localStorage.getItem("runtime.annotation.ins_ann_test")).toBeTruthy();
    clearAnnotations(iid);
    expect(readAnnotations(iid)).toEqual([]);
    expect(localStorage.getItem("runtime.annotation.ins_ann_test")).toBeNull();
  });

  it("notifies subscribers on every mutation", () => {
    let hits = 0;
    const unsubscribe = subscribeAnnotations(() => {
      hits += 1;
    });
    addAnnotation(iid, createCardDraft("x", { now: 1, id: "x" }));
    removeAnnotation(iid, "x");
    clearAnnotations(iid);
    expect(hits).toBe(3);
    unsubscribe();
  });

  it("survives malformed JSON without throwing", () => {
    localStorage.setItem("runtime.annotation.ins_ann_test", "{not json");
    expect(readAnnotations(iid)).toEqual([]);
  });
});

describe("readAnnotationSelection", () => {
  function mount(html: string): HTMLElement {
    document.body.innerHTML = `<div data-annotation-instance="ins_selected" data-annotation-readonly="0">${html}</div>`;
    return document.body.firstElementChild as HTMLElement;
  }

  function selectText(el: Element): void {
    const range = document.createRange();
    range.selectNodeContents(el);
    const selection = window.getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);
  }

  afterEach(() => {
    document.body.innerHTML = "";
    window.getSelection()?.removeAllRanges();
  });

  it("captures a transcript anchor with the owning session and message id", () => {
    const root = mount(
      '<section data-anchor-surface="transcript" data-anchor-message="msg_42">ship on Friday please</section>',
    );
    selectText(root.querySelector("[data-anchor-surface]")!);
    expect(readAnnotationSelection()).toEqual({
      instanceId: "ins_selected",
      surface: "transcript",
      messageId: "msg_42",
      quote: "ship on Friday please",
      readonly: false,
    });
  });

  it("captures a task-detail anchor whose instance lives on the surface", () => {
    document.body.innerHTML =
      '<section data-anchor-surface="task-detail" data-annotation-instance="ins_board" data-annotation-readonly="0"><p>mandate prose here</p></section>';
    selectText(document.querySelector("p")!);
    expect(readAnnotationSelection()?.surface).toBe("task-detail");
    expect(readAnnotationSelection()?.instanceId).toBe("ins_board");
  });

  it("returns nothing outside an anchor surface", () => {
    mount("<p>just some page text</p>");
    selectText(document.querySelector("p")!);
    expect(readAnnotationSelection()).toBeNull();
  });

  it("reports a read-only (archived) session so the affordance hides", () => {
    document.body.innerHTML =
      '<div data-annotation-instance="ins_arch" data-annotation-readonly="1"><section data-anchor-surface="transcript" data-anchor-message="m1">frozen text</section></div>';
    selectText(document.querySelector("section")!);
    expect(readAnnotationSelection()?.readonly).toBe(true);
  });

  it("returns nothing without an owning session", () => {
    document.body.innerHTML =
      '<section data-anchor-surface="transcript" data-anchor-message="m1">orphan text</section>';
    selectText(document.querySelector("section")!);
    expect(readAnnotationSelection()).toBeNull();
  });
});
