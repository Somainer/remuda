import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AnchorText, type AnchorAttachment } from "./AnchorText";

/** Inline [Image #n] tokens render as thumbnail chips linking the preview. */

const attachments: AnchorAttachment[] = [
  { objectId: "obj_1", name: "a.png", previewUrl: "blob:a", index: 1 },
  { objectId: "obj_2", name: "b.png", previewUrl: "blob:b", index: 2 },
];

describe("AnchorText", () => {
  it("renders each token as an inline thumbnail link", () => {
    render(
      <AnchorText
        text="compare [Image #1] with [Image #2] please"
        attachments={attachments}
      />,
    );
    const chips = screen.getAllByTestId("inline-image-anchor");
    expect(chips).toHaveLength(2);
    expect(chips[0].getAttribute("data-index")).toBe("1");
    expect(chips[0].getAttribute("href")).toBe("blob:a");
    expect(chips[1].getAttribute("data-index")).toBe("2");
    expect(screen.getByTestId("anchor-text").textContent).toContain("compare");
    expect(screen.getByTestId("anchor-text").textContent).toContain("with");
  });

  it("keeps an unmatched token as text rather than dropping it", () => {
    render(<AnchorText text="where did [Image #3] go?" attachments={attachments.slice(0, 1)} />);
    expect(screen.queryAllByTestId("inline-image-anchor")).toHaveLength(0);
    const token = screen.getByTestId("inline-image-token");
    expect(token.textContent).toBe("[Image #3]");
  });

  it("renders plain text unchanged", () => {
    render(<AnchorText text="just words" attachments={attachments} />);
    expect(screen.queryByTestId("inline-image-anchor")).toBeNull();
    expect(screen.getByText("just words")).toBeTruthy();
  });
});
