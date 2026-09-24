import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { formatStaleAge, StaleScreenBadge } from "./StaleScreenBadge";

describe("stale screen badge", () => {
  it("reads the age and the reason, and never claims the session is live", () => {
    render(
      <StaleScreenBadge
        stale={{ ageMs: 26 * 60 * 1000, reason: "instance-gone" }}
      />,
    );
    const badge = screen.getByTestId("tty-stale");
    expect(badge).toHaveTextContent("26 分钟前");
    expect(badge).toHaveTextContent("会话已结束");
    // 运行中 over a frozen frame is the exact bug this badge exists to end.
    expect(badge).not.toHaveTextContent("运行中");
    expect(badge).toHaveAttribute("data-stale-reason", "instance-gone");
    expect(badge).toHaveAttribute("data-stale-age-ms", String(26 * 60 * 1000));
  });

  it("says the age is unknown rather than counting from now", () => {
    render(<StaleScreenBadge stale={{ reason: "node-link-unavailable" }} />);
    const badge = screen.getByTestId("tty-stale");
    expect(badge).toHaveTextContent("时间未知");
    expect(badge).toHaveAttribute("data-stale-age-ms", "unknown");
    // Freshness UNKNOWN, not a verified end: 「画面可能过期」, never 「会话已结束」.
    expect(badge).toHaveTextContent("画面可能过期");
    expect(badge).toHaveTextContent("Node 连接不可用");
    expect(badge).not.toHaveTextContent("会话已结束");
  });

  it("makes no claim about a reason code it does not know", () => {
    // An unrecognised code must never be rendered as "the session ended" —
    // that would tell an operator to give up on a session that may still be
    // alive. Rendering nothing for it is the safe half of that rule.
    render(
      <StaleScreenBadge stale={{ ageMs: 1000, reason: "some-future-code" }} />,
    );
    const badge = screen.getByTestId("tty-stale");
    expect(badge).not.toHaveTextContent("会话已结束");
    expect(badge).not.toHaveTextContent("Node 连接不可用");
    expect(badge).toHaveTextContent("画面已停更");
    expect(badge).toHaveAttribute("data-stale-reason", "some-future-code");
  });

  it("scales the age into the unit an operator would say out loud", () => {
    expect(formatStaleAge(0)).toBe("0 秒");
    expect(formatStaleAge(45 * 1000)).toBe("45 秒");
    expect(formatStaleAge(90 * 1000)).toBe("1 分钟");
    expect(formatStaleAge(3 * 60 * 60 * 1000)).toBe("3 小时");
    expect(formatStaleAge(50 * 60 * 60 * 1000)).toBe("2 天");
  });
});
