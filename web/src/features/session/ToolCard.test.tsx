import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { known, type Id } from "../../types/wire";
import { ToolCard } from "./ToolCard";

function mcpCall(): ToolCallPayload {
  return {
    nodeId: "obj_n" as Id,
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: "obj_cua_1" as Id,
    parentToolCallId: null,
    toolName: known("mcp__codex-computer-use__get_app_state"),
    displayTitle: known("mcp__codex-computer-use__get_app_state"),
    category: "mcp",
    input: known({ app: "com.apple.Safari" }),
    inputTextDelta: null,
    state: "running",
    executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
  };
}

function imageResult(): ToolResultPayload {
  return {
    nodeId: "obj_cua_1" as Id,
    revision: "2",
    operation: "close",
    baseRevision: "1",
    toolCallId: "obj_cua_1" as Id,
    stage: "final",
    outcome: "succeeded",
    blocks: [
      { type: "text", text: "window state captured" },
      {
        type: "image",
        objectId: "obj_shot_1" as Id,
        mediaType: "image/png",
        name: "screen-1.png",
        size: 70,
      },
    ],
    structuredResult: known({}),
    exitCode: known(0),
    changes: [],
  };
}

describe("ToolCard · image tool result (D-045 §6.2)", () => {
  it("renders a bounded thumbnail that links to the object route, with the block name as alt", () => {
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={imageResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    const image = screen.getByTestId("tool-thumb") as HTMLImageElement;
    expect(image.getAttribute("src")).toBe("/v1/objects/obj_shot_1");
    expect(image.alt).toBe("screen-1.png");
    expect(image.getAttribute("loading")).toBe("lazy");
    // The click target opens the object route; no lightbox/auto-expand.
    const link = screen.getByTestId("tool-media-link") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("/v1/objects/obj_shot_1");
    expect(link.target).toBe("_blank");
    // The text half of the result is still present.
    expect(screen.getByText("window state captured")).toBeTruthy();
    // The MCP card keeps the tool's server/tool identity.
    expect(screen.getByText(/codex-computer-use/)).toBeTruthy();
  });

  it("renders no media row for a text-only result", () => {
    const textOnly = imageResult();
    textOnly.blocks = [{ type: "text", text: "plain result" }];
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={textOnly}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.queryByTestId("tool-media")).toBeNull();
    expect(screen.queryByTestId("tool-thumb")).toBeNull();
  });

  it("swaps an expired object to the fixed not-attached note instead of a broken image", () => {
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={imageResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    const img = screen.getByTestId("tool-thumb") as HTMLImageElement;
    // Simulate the object route 404/expiring (24 h TTL or budget sweep).
    fireEvent.error(img);
    expect(screen.queryByTestId("tool-thumb")).toBeNull();
    expect(screen.getByText(/image not attached: image\/png/)).toBeTruthy();
  });
});

describe("ToolCard · reading-column fold (D-053)", () => {
  it("folds a settled card at desktop width inside the reading column", () => {
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={imageResult()}
        completeness="structured"
        diffState="unknown"
        foldSettled
      />,
    );
    const card = screen.getByTestId("tool-card");
    expect(card.getAttribute("data-folded")).toBe("1");
    expect(card.hasAttribute("data-running")).toBe(false);
    expect(screen.queryByTestId("tool-fold-partial")).toBeNull();
    fireEvent.click(screen.getByTestId("tool-fold-open"));
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    expect(screen.getByTestId("tool-thumb")).toBeTruthy();
  });

  it("keeps 不完整 visible on a folded partial record", () => {
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={imageResult()}
        completeness="partial"
        diffState="unknown"
        foldSettled
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");
    expect(screen.getByTestId("tool-fold-partial").textContent).toBe("不完整");
  });

  it("never folds a failed card", () => {
    const failed = imageResult();
    failed.outcome = "failed";
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={failed}
        completeness="structured"
        diffState="unknown"
        foldSettled
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
  });

  it("leaves a running card open", () => {
    render(
      <ToolCard
        driverKind="claude-pty"
        call={mcpCall()}
        result={null}
        completeness="structured"
        diffState="unknown"
        foldSettled
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
  });
});
