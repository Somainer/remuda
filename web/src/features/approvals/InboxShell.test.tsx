import { act, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DepartedRow } from "./InboxShell";
import type { DecisionView } from "./ApprovalCard";
import type { Interaction } from "../../types/interaction";

/**
 * c-inboxperf / UO-9 round-2: a 2 s interaction.list poll re-parses into
 * fresh objects and the parent builds a fresh `view` every time. Departed
 * rows are not actionable, so an unchanged 已离队 row must NOT commit on a
 * poll — the memo comparator on `sig` is what prevents the churn.
 */

type DepartedView = DecisionView & { stateText: string; previewText: string; title: string };

function departedView(over: Partial<DepartedView> = {}): DepartedView {
  const item = {
    id: "itx_expired",
    state: "expired",
  } as unknown as Interaction;
  const base: DepartedView = {
    key: "itx_expired",
    sig: "sig-1",
    item,
    uiState: "pending",
    focused: false,
    timeLabel: "12:04",
    hostLabel: "bolt",
    workspaceLabel: "sfe-root",
    instanceKind: "claude",
    stateText: "过期，未作用于新进程",
    title: "Bash",
    previewText: "rm -rf /tmp/x",
    ...over,
  };
  return base;
}

function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

describe("DepartedRow poll memoization", () => {
  afterEach(() => vi.restoreAllMocks());

  it("does not commit when a poll rebuilds an equal-sig view", async () => {
    const { container, rerender } = render(<DepartedRow view={departedView()} />);
    const article = container.firstChild as HTMLElement;
    const records: MutationRecord[] = [];
    const observer = new MutationObserver((batch) => records.push(...batch));
    observer.observe(article, {
      subtree: true,
      childList: true,
      characterData: true,
      attributes: true,
    });

    // A fresh object identity (as the 2 s poll produces) but an equal sig.
    act(() => rerender(<DepartedRow view={departedView()} />));
    await act(flush);
    expect(records).toHaveLength(0);

    // A genuinely changed state commits (so real updates still render).
    act(() =>
      rerender(
        <DepartedRow
          view={departedView({ sig: "sig-2", stateText: "已在其它设备处理" })}
        />,
      ),
    );
    await act(flush);
    expect(records.length).toBeGreaterThan(0);
    observer.disconnect();
  });

  it("skips the commit for N unchanged departed rows when one new interaction arrives", async () => {
    // Simulate the queue scenario: three existing rows re-render with fresh
    // equal-sig views (a new unrelated interaction arrived elsewhere), and
    // assert none of their subtrees mutate.
    const views = [departedView({ key: "a" }), departedView({ key: "b" }), departedView({ key: "c" })];
    const rendered = views.map((view) => render(<DepartedRow view={view} />));
    const articles = rendered.map((r) => r.container.firstChild as HTMLElement);
    const counts = articles.map(() => 0);
    const observers = articles.map((article, i) => {
      const observer = new MutationObserver(() => {
        counts[i] += 1;
      });
      observer.observe(article, { subtree: true, childList: true, characterData: true });
      return observer;
    });

    rendered.forEach((r, i) => act(() => r.rerender(<DepartedRow view={{ ...views[i]! }} />)));
    await act(flush);
    expect(counts).toEqual([0, 0, 0]);
    observers.forEach((observer) => observer.disconnect());
    rendered.forEach((r) => r.unmount());
  });
});
