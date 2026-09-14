import { beforeEach, describe, expect, it } from "vitest";
import {
  clearNewSessionDraft,
  draftAuthSubject,
  isEmptyDraft,
  loadNewSessionDraft,
  memoryDraftCountForTest,
  newSessionDraftKey,
  sanitizeDraft,
  saveNewSessionDraft,
  type NewSessionDraft,
} from "./newSessionDraft";

const alice = "dev_alice";
const bob = "dev_bob";
const host1 = "hst_1";
const host2 = "hst_2";
const wsp1 = "wsp_1";
const wsp2 = "wsp_2";

function body(over: Partial<NewSessionDraft> = {}): NewSessionDraft {
  return { prompt: "查一下 spill 原因", ...over };
}

/** Every localStorage key the draft layer is allowed to create. */
function draftKeys(): string[] {
  return Object.keys(localStorage).filter((key) => key.startsWith("runtime.draft.new."));
}

beforeEach(() => {
  localStorage.clear();
  // The memory map is module state; clear the tuples this file uses.
  for (const host of [host1, host2]) for (const wsp of [wsp1, wsp2]) {
    clearNewSessionDraft(null, host, wsp);
    clearNewSessionDraft(alice, host, wsp);
    clearNewSessionDraft(bob, host, wsp);
  }
});

describe("draftAuthSubject", () => {
  it("uses the Hub-issued device id when a session exists", () => {
    expect(draftAuthSubject({ deviceId: "dev_1", name: "n", token: "" })).toBe("dev_1");
  });

  it("is null when logged out or the id is blank, which selects memory-only drafts", () => {
    expect(draftAuthSubject(null)).toBeNull();
    expect(draftAuthSubject({ deviceId: "  ", name: "n", token: "" })).toBeNull();
  });
});

describe("key namespacing", () => {
  it("builds the versioned, fully-qualified key", () => {
    expect(newSessionDraftKey(alice, host1, wsp1)).toBe(
      "runtime.draft.new.v1.dev_alice.hst_1.wsp_1",
    );
  });

  it("neutralizes characters in ids so they cannot forge another namespace", () => {
    const key = newSessionDraftKey("dev/x!!", "h", "w");
    expect(key).toBe("runtime.draft.new.v1.dev_x__.h.w");
    expect(key.startsWith("runtime.draft.new.v1.")).toBe(true);
  });
});

describe("persistence with a reliable identity", () => {
  it("round-trips the prompt and non-sensitive options", () => {
    const draft = body({
      kind: "claude",
      model: "passthrough/auto",
      permissionMode: "acceptEdits",
      delegation: "host",
      cwdMode: "worktree",
      cwdPath: "src",
      worktreeName: "feat-x",
      effortKind: "claude",
      effortIndex: 3,
      effortUltracode: false,
    });
    saveNewSessionDraft(alice, host1, wsp1, draft);
    expect(loadNewSessionDraft(alice, host1, wsp1)).toEqual(draft);
  });

  it("isolates authenticated subjects on the same host and workspace", () => {
    saveNewSessionDraft(alice, host1, wsp1, body({ prompt: "alice secret" }));
    saveNewSessionDraft(bob, host1, wsp1, body({ prompt: "bob secret" }));
    expect(loadNewSessionDraft(alice, host1, wsp1)?.prompt).toBe("alice secret");
    expect(loadNewSessionDraft(bob, host1, wsp1)?.prompt).toBe("bob secret");
  });

  it("isolates hosts and workspaces for the same subject", () => {
    saveNewSessionDraft(alice, host1, wsp1, body({ prompt: "one" }));
    saveNewSessionDraft(alice, host1, wsp2, body({ prompt: "two" }));
    saveNewSessionDraft(alice, host2, wsp1, body({ prompt: "three" }));
    expect(loadNewSessionDraft(alice, host1, wsp1)?.prompt).toBe("one");
    expect(loadNewSessionDraft(alice, host1, wsp2)?.prompt).toBe("two");
    expect(loadNewSessionDraft(alice, host2, wsp1)?.prompt).toBe("three");
    expect(draftKeys()).toHaveLength(3);
  });

  it("removes the stored draft when it becomes empty", () => {
    saveNewSessionDraft(alice, host1, wsp1, body());
    expect(draftKeys()).toHaveLength(1);
    saveNewSessionDraft(alice, host1, wsp1, { prompt: "   ", cwdPath: "", worktreeName: "" });
    expect(draftKeys()).toHaveLength(0);
    expect(loadNewSessionDraft(alice, host1, wsp1)).toBeNull();
  });

  it("clear only removes the requested context", () => {
    saveNewSessionDraft(alice, host1, wsp1, body());
    saveNewSessionDraft(alice, host1, wsp2, body());
    clearNewSessionDraft(alice, host1, wsp1);
    expect(loadNewSessionDraft(alice, host1, wsp1)).toBeNull();
    expect(loadNewSessionDraft(alice, host1, wsp2)?.prompt).toBeTruthy();
  });
});

describe("memory-only drafts without identity", () => {
  it("never writes localStorage but keeps the draft for the tab lifetime", () => {
    const result = saveNewSessionDraft(null, host1, wsp1, body());
    expect(result.persisted).toBe(false);
    expect(draftKeys()).toHaveLength(0);
    expect(loadNewSessionDraft(null, host1, wsp1)?.prompt).toBe(body().prompt);
    expect(memoryDraftCountForTest()).toBe(1);
  });

  it("keeps anonymous contexts separated by host and workspace", () => {
    saveNewSessionDraft(null, host1, wsp1, body({ prompt: "a" }));
    saveNewSessionDraft(null, host1, wsp2, body({ prompt: "b" }));
    expect(loadNewSessionDraft(null, host1, wsp1)?.prompt).toBe("a");
    expect(loadNewSessionDraft(null, host1, wsp2)?.prompt).toBe("b");
  });

  it("clears the in-memory draft on explicit discard", () => {
    saveNewSessionDraft(null, host1, wsp1, body());
    clearNewSessionDraft(null, host1, wsp1);
    expect(loadNewSessionDraft(null, host1, wsp1)).toBeNull();
    expect(draftKeys()).toHaveLength(0);
  });

  it("does not promote an anonymous draft to storage when a subject later appears", () => {
    saveNewSessionDraft(null, host1, wsp1, body({ prompt: "anon" }));
    // A different (now authenticated) context must not inherit the anonymous one.
    expect(loadNewSessionDraft(alice, host1, wsp1)).toBeNull();
    expect(draftKeys()).toHaveLength(0);
  });
});

describe("sensitive data is never persisted", () => {
  it("strips unknown fields even when the caller forces them through", () => {
    const hostile = {
      ...body(),
      token: "sk-secret",
      claudeConfigDir: "/home/alice/.claude",
      settingsOverlayPath: "/tmp/overlay.json",
      binaryPath: "/opt/claude",
      maxBudgetUsd: "999",
      args: ["--add-dir", "/srv"],
      attachments: [{ name: "key.png", bytes: "AAAA" }],
    } as unknown as NewSessionDraft;
    saveNewSessionDraft(alice, host1, wsp1, hostile);
    const raw = localStorage.getItem(newSessionDraftKey(alice, host1, wsp1)) ?? "";
    expect(raw).not.toContain("sk-secret");
    expect(raw).not.toContain("claudeConfigDir");
    expect(raw).not.toContain("settingsOverlayPath");
    expect(raw).not.toContain("binaryPath");
    expect(raw).not.toContain("maxBudgetUsd");
    expect(raw).not.toContain("attachments");
    const restored = loadNewSessionDraft(alice, host1, wsp1)!;
    expect(restored.prompt).toBe(body().prompt);
    expect(Object.keys(restored).sort()).toEqual(["prompt"]);
  });

  it("sanitizes a tampered localStorage entry on read", () => {
    localStorage.setItem(
      newSessionDraftKey(alice, host1, wsp1),
      JSON.stringify({
        v: 1,
        prompt: "p",
        token: "sk-leaked",
        settingsOverlayPath: "/tmp/x",
        nested: { secret: 1 },
      }),
    );
    const draft = loadNewSessionDraft(alice, host1, wsp1)!;
    expect(draft.prompt).toBe("p");
    expect(Object.keys(draft)).toEqual(["prompt"]);
  });

  it("rejects storage written by another draft version or with a broken shape", () => {
    const key = newSessionDraftKey(alice, host1, wsp1);
    localStorage.setItem(key, JSON.stringify({ v: 2, prompt: "future" }));
    expect(loadNewSessionDraft(alice, host1, wsp1)).toBeNull();
    localStorage.setItem(key, "{not json");
    expect(loadNewSessionDraft(alice, host1, wsp1)).toBeNull();
    localStorage.setItem(key, JSON.stringify({ v: 1 }));
    expect(loadNewSessionDraft(alice, host1, wsp1)).toBeNull();
  });

  it("sanitizeDraft caps prompt length and clamps effort index", () => {
    const draft = sanitizeDraft({
      v: 1,
      prompt: "x".repeat(200_000),
      effortIndex: 999,
      effortUltracode: "yes",
      cwdMode: "sideways",
    })!;
    expect(draft.prompt).toHaveLength(100_000);
    expect(draft.effortIndex).toBe(20);
    expect(draft.effortUltracode).toBeUndefined();
    expect(draft.cwdMode).toBeUndefined();
  });
});

describe("coexistence with composer drafts", () => {
  it("never touches runtime.draft.<instanceId> keys on save, load or clear", () => {
    const composerKey = "runtime.draft.inst_composer";
    localStorage.setItem(composerKey, "half-written composer text");
    saveNewSessionDraft(alice, host1, wsp1, body());
    expect(localStorage.getItem(composerKey)).toBe("half-written composer text");
    loadNewSessionDraft(alice, host1, wsp1);
    clearNewSessionDraft(alice, host1, wsp1);
    expect(localStorage.getItem(composerKey)).toBe("half-written composer text");
  });
});

describe("isEmptyDraft", () => {
  it("treats whitespace-only bodies as empty", () => {
    expect(isEmptyDraft({ prompt: "" })).toBe(true);
    expect(isEmptyDraft({ prompt: "  \n" })).toBe(true);
    expect(isEmptyDraft({ prompt: "x" })).toBe(false);
    expect(isEmptyDraft({ prompt: "", worktreeName: "wt" })).toBe(false);
  });
});
