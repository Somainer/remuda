import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { HostProviderBinding, formatProviderBinding, parseProviderBinding } from "./HostProviderBinding";

const profiles = [
  { id: "pvp_u", name: "uni-gw", scope: "universal" },
  { id: "pvp_h", name: "host-gw", scope: "host:hst_a" },
];

describe("HostProviderBinding", () => {
  it("parses and formats binding values", () => {
    expect(parseProviderBinding(undefined)).toEqual({ kind: "auto", profileId: "" });
    expect(parseProviderBinding("native")).toEqual({ kind: "native", profileId: "" });
    expect(parseProviderBinding("profile:pvp_u")).toEqual({ kind: "profile", profileId: "pvp_u" });
    expect(formatProviderBinding("auto", "")).toBe("auto");
    expect(formatProviderBinding("profile", "pvp_u")).toBe("profile:pvp_u");
  });

  it("lets the operator pick auto, native, or a stored profile", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const { rerender } = render(
      <HostProviderBinding binding="auto" profiles={profiles} onChange={onChange} />,
    );
    await user.click(screen.getByTestId("host-binding-native"));
    expect(onChange).toHaveBeenCalledWith("native");
    rerender(<HostProviderBinding binding="native" profiles={profiles} onChange={onChange} />);
    await user.click(screen.getByTestId("host-binding-profile"));
    expect(onChange).toHaveBeenCalledWith("profile:pvp_u");
    rerender(<HostProviderBinding binding="profile:pvp_u" profiles={profiles} onChange={onChange} />);
    await user.selectOptions(screen.getByTestId("host-binding-profile-id"), "pvp_h");
    expect(onChange).toHaveBeenCalledWith("profile:pvp_h");
  });
});
