import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  AnnotationBadge,
  AnnotationPanel,
  AnnotationProvider,
  useAnnotationsContext,
} from "./AnnotationPanel";
import { readAnnotations } from "./annotations";

/**
 * Plan task-model task 9: the badge count and the two-tab panel over the
 * device-local drafts. Storage/serialisation are covered in
 * annotations.test.ts; this locks the React wiring (badge appears with N,
 * panel adds/removes, read-only archived sessions cannot annotate).
 */

const iid = "ins_panel";

function seed(drafts: unknown[]) {
  localStorage.setItem(`runtime.annotation.${iid}`, JSON.stringify(drafts));
}

function Harness({
  readonly = false,
  taskId = null,
  taskTitle = null,
  queuedAnchor = null,
}: {
  readonly?: boolean;
  taskId?: string | null;
  taskTitle?: string | null;
  queuedAnchor?: { surface: "task-detail" | "transcript"; messageId: string | null; quote: string } | null;
}) {
  return (
    <AnnotationProvider>
      <AnnotationBadge instanceId={iid} readonly={readonly} />
      <AnnotationPanel instanceId={iid} taskId={taskId} taskTitle={taskTitle} readonly={readonly} />
      <QueueButton tab="card" anchor={null} />
      {queuedAnchor ? <QueueButton tab="anchor" anchor={queuedAnchor} /> : null}
    </AnnotationProvider>
  );
}

function QueueButton({
  tab,
  anchor,
}: {
  tab: "card" | "anchor";
  anchor: { surface: "task-detail" | "transcript"; messageId: string | null; quote: string } | null;
}) {
  const ctx = useAnnotationsContext();
  return (
    <button
      type="button"
      data-testid={tab === "card" ? "open-card" : "queue-anchor"}
      onClick={() => ctx.openPanel(iid, tab, anchor)}
    >
      open
    </button>
  );
}

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("AnnotationBadge + AnnotationPanel", () => {
  it("shows no badge with zero drafts and counts the drafts the next send carries", () => {
    const { rerender } = render(<Harness />);
    expect(screen.queryByTestId("annotation-badge")).toBeNull();

    seed([
      { id: "a1", createdAt: 1, carrier: "card", body: "card note", taskId: "tsk_1" },
      {
        id: "a2",
        createdAt: 2,
        carrier: "anchor",
        body: "anchor note",
        anchor: { surface: "transcript", messageId: "m1", quote: "q" },
      },
    ]);
    rerender(<Harness />);
    expect(screen.getByTestId("annotation-badge-count").textContent).toBe("2");
    expect(screen.getByTestId("annotation-badge")).toHaveTextContent("本次发送带 2 条批注");
  });

  it("adds a card draft through the panel and removes it", () => {
    render(<Harness taskId="tsk_7" taskTitle="SE-07 flake" />);
    fireEvent.click(screen.getByTestId("open-card"));

    fireEvent.change(screen.getByTestId("annotation-card-input"), {
      target: { value: "please rerun the gate" },
    });
    fireEvent.click(screen.getByTestId("annotation-card-save"));

    expect(readAnnotations(iid)).toHaveLength(1);
    expect(readAnnotations(iid)[0]).toMatchObject({
      carrier: "card",
      body: "please rerun the gate",
      taskId: "tsk_7",
      taskTitle: "SE-07 flake",
    });
    expect(screen.getByTestId("annotation-item")).toHaveTextContent("please rerun the gate");
    expect(screen.getByTestId("annotation-badge-count").textContent).toBe("1");

    fireEvent.click(screen.getByTestId("annotation-item-remove"));
    expect(readAnnotations(iid)).toHaveLength(0);
    expect(screen.queryByTestId("annotation-badge")).toBeNull();
  });

  it("saves an anchor draft into the 标记 tab and shows the ① numbering", () => {
    render(
      <Harness
        queuedAnchor={{ surface: "task-detail", messageId: null, quote: "mandate says friday" }}
      />,
    );
    fireEvent.click(screen.getByTestId("queue-anchor"));
    expect(screen.getByTestId("annotation-anchor-form")).toHaveTextContent("mandate says friday");
    fireEvent.change(screen.getByTestId("annotation-anchor-input"), {
      target: { value: "date slipped" },
    });
    fireEvent.click(screen.getByTestId("annotation-anchor-save"));

    const drafts = readAnnotations(iid);
    expect(drafts).toHaveLength(1);
    expect(drafts[0].anchor?.surface).toBe("task-detail");
    fireEvent.click(screen.getByTestId("annotation-tab-anchor"));
    expect(screen.getByTestId("annotation-item")).toHaveTextContent("①");
  });

  it("is a read-only preview for archived-task sessions: no create forms, drafts still removable", () => {
    seed([{ id: "a1", createdAt: 1, carrier: "card", body: "kept" }]);
    render(<Harness readonly />);
    const badge = screen.getByTestId("annotation-badge");
    // Not disabled: a draft made before the archived state resolved stays
    // openable for review/removal, but creating is blocked.
    expect(badge).not.toBeDisabled();
    expect(badge).toHaveAttribute("data-readonly", "1");
    fireEvent.click(screen.getByTestId("open-card"));
    expect(screen.queryByTestId("annotation-card-input")).toBeNull();
    expect(screen.getByTestId("annotation-readonly-note")).toHaveTextContent("只读预览");
    expect(screen.getByTestId("annotation-panel")).toHaveAttribute("data-readonly", "1");
    // Existing drafts can still be withdrawn (otherwise they would be trapped
    // behind a preview forever).
    fireEvent.click(screen.getByTestId("annotation-item-remove"));
    expect(readAnnotations(iid)).toHaveLength(0);
  });
});
