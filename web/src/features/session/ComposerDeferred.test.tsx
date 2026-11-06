import { render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import { deferredLabel, effortAt } from "./effort";

/**
 * cb-command-queue: a configure the Hub answers `deferred` reads
 * 下一回合生效 (the next idle moment) or 下一章节生效 (the next launch) on the
 * effort chip, instead of a prediction from the session's activity.
 */
it("labels a change deferred to the next turn", () => {
  render(
    <Composer
      instanceId="ins_deferred_turn"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      model="opus"
      effort={effortAt("claude", 3, false)}
      effortEffective={null}
      effortPending={{ name: "high", ultracode: false, queued: true, deferred: "next-turn" }}
      onEffort={vi.fn()}
    />,
  );
  const tag = screen.getByTestId("model-effort-pending");
  expect(tag).toHaveTextContent("下一回合生效");
  expect(tag).toHaveAttribute("data-deferred", "next-turn");
  expect(screen.getByTestId("model-effort-chip")).toHaveAttribute(
    "title",
    expect.stringContaining("下一回合生效"),
  );
});

it("keeps the predicted 排队中 when the Hub said nothing", () => {
  render(
    <Composer
      instanceId="ins_deferred_none"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      model="opus"
      effort={effortAt("claude", 3, false)}
      effortEffective={null}
      effortPending={{ name: "high", ultracode: false, queued: true }}
      onEffort={vi.fn()}
    />,
  );
  expect(screen.getByTestId("model-effort-pending")).toHaveTextContent("排队中");
});

it("labels a change deferred to the next chapter with nothing pending", () => {
  render(
    <Composer
      instanceId="ins_deferred_chapter"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      model="opus"
      effort={effortAt("claude", 3, false)}
      effortEffective={null}
      effortPending={null}
      effortDeferred="next-chapter"
      onEffort={vi.fn()}
    />,
  );
  const tag = screen.getByTestId("model-effort-pending");
  expect(tag).toHaveTextContent("下一章节生效");
  expect(tag).toHaveAttribute("data-deferred", "next-chapter");
});

it("names both boundaries", () => {
  expect(deferredLabel("next-turn")).toBe("下一回合生效");
  expect(deferredLabel("next-chapter")).toBe("下一章节生效");
});
