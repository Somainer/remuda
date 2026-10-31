import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { RootTurnTag } from "./RootTurnTag";
import { rootTurnTagText } from "./rootTurnTag";

describe("rootTurnTagText", () => {
  it("labels only a turn the owner did not start, per surface", () => {
    const agent = { commandId: "cmd_1", origin: "agent", continuation: "native" } as const;
    const bot = { commandId: "cmd_2", origin: "bot", continuation: "none" } as const;
    const human = { commandId: "cmd_3", origin: "human", continuation: "none" } as const;
    expect(rootTurnTagText(agent, "main")).toBe("Main 自主");
    expect(rootTurnTagText(agent, "approvals")).toBe("agent 自发");
    expect(rootTurnTagText(bot, "main")).toBe("Main 自主");
    expect(rootTurnTagText(human, "main")).toBeNull();
    // An owner turn on a carrier without boundaries is still the owner's.
    expect(rootTurnTagText({ ...human, continuation: "unknown" }, "approvals")).toBeNull();
    expect(rootTurnTagText(null, "main")).toBeNull();
    expect(rootTurnTagText(undefined, "approvals")).toBeNull();
  });

  it("renders the Main wording beside an initiator display", () => {
    render(
      <RootTurnTag
        rootTurn={{ commandId: "cmd_1", origin: "agent", continuation: "native" }}
        surface="main"
      />,
    );
    const tag = screen.getByTestId("root-turn-tag");
    expect(tag).toHaveTextContent("Main 自主");
    expect(tag.getAttribute("title")).toContain("cmd_1");
  });
});
