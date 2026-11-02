import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { RequestedByNote, RequestedByTag } from "./RequestedByNote";
import { requestedByExcerpt, requestedByTagText } from "./requestedBy";

describe("requestedByExcerpt", () => {
  it("keeps the owner's words and drops blank ones", () => {
    expect(
      requestedByExcerpt({
        commandId: "cmd_1",
        excerpt: "dispatch a worker on the other node",
      }),
    ).toBe("dispatch a worker on the other node");
    expect(requestedByExcerpt({ excerpt: "   " })).toBeNull();
    expect(requestedByExcerpt({ commandId: "cmd_1" })).toBeNull();
    expect(requestedByExcerpt(null)).toBeNull();
    expect(requestedByExcerpt(undefined)).toBeNull();
  });
});

describe("requestedByTagText", () => {
  it("labels only a turn the owner did not start", () => {
    expect(requestedByTagText({ origin: "agent" })).toBe("agent 自发");
    expect(requestedByTagText({ origin: "hub" })).toBe("agent 自发");
    expect(requestedByTagText({ origin: "bot" })).toBe("agent 自发");
    expect(requestedByTagText({ commandId: "cmd_owner", excerpt: "hi" })).toBeNull();
    expect(requestedByTagText({ origin: "human" })).toBeNull();
    expect(requestedByTagText(null)).toBeNull();
    expect(requestedByTagText(undefined)).toBeNull();
  });
});

describe("RequestedByNote", () => {
  it("shows the owner's own words with the recorded command in the title", () => {
    render(
      <RequestedByNote
        requestedBy={{ commandId: "cmd_owner", excerpt: "ship it behind a flag" }}
      />,
    );
    const note = screen.getByTestId("requested-by");
    expect(note).toHaveTextContent("你说：ship it behind a flag");
    expect(note.getAttribute("title")).toContain("cmd_owner");
    expect(note.getAttribute("title")).toContain("仅作记录");
  });

  it("renders nothing without an excerpt (an agent-origin turn)", () => {
    render(<RequestedByNote requestedBy={{ origin: "agent" }} />);
    expect(screen.queryByTestId("requested-by")).toBeNull();
  });
});

describe("RequestedByTag", () => {
  it("labels a non-owner turn and names its origin", () => {
    render(<RequestedByTag requestedBy={{ origin: "hub" }} />);
    const tag = screen.getByTestId("requested-by-tag");
    expect(tag).toHaveTextContent("agent 自发");
    expect(tag).toHaveAttribute("data-origin", "hub");
    expect(tag.getAttribute("title")).toContain("来源 hub");
    expect(tag.getAttribute("title")).toContain("仅作记录");
  });

  it("renders nothing for an owner turn or no record", () => {
    const { unmount } = render(
      <RequestedByTag requestedBy={{ commandId: "cmd_owner", excerpt: "hi" }} />,
    );
    expect(screen.queryByTestId("requested-by-tag")).toBeNull();
    unmount();
    render(<RequestedByTag requestedBy={null} />);
    expect(screen.queryByTestId("requested-by-tag")).toBeNull();
  });
});
