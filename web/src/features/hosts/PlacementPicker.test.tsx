import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import { HOST_FIXTURES } from "./fixtures";
import { PlacementPicker } from "./PlacementPicker";
import type { Placement } from "./model";

function Harness() {
  const [value, setValue] = useState<Placement>({ kind: "any" });
  return <PlacementPicker hosts={HOST_FIXTURES} value={value} onChange={setValue} />;
}

describe("PlacementPicker", () => {
  it("switches host / labels / any", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    expect(screen.getByTestId("placement-match-count")).toBeTruthy();
    await user.click(screen.getByTestId("placement-kind-labels"));
    await user.click(screen.getByTestId("placement-label-region:sg"));
    await user.click(screen.getByTestId("placement-kind-host"));
    expect(screen.getByTestId("placement-host")).toBeTruthy();
    await user.click(screen.getByTestId("placement-kind-any"));
    expect(screen.getByTestId("placement-match-count").textContent).toMatch(/可调度/);
  });
});
