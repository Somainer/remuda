import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ProviderForm } from "./ProviderForm";

describe("ProviderForm", () => {
  it("submits a gateway profile without echoing the token in the name field", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(<ProviderForm onSubmit={onSubmit} onCancel={() => undefined} />);
    await user.type(screen.getByTestId("provider-name"), "my gw");
    await user.type(screen.getByTestId("provider-base-url"), "http://127.0.0.1:1");
    await user.type(screen.getByTestId("provider-token"), "sk-dummy-token-zzzz");
    await user.type(screen.getByTestId("provider-models"), "passthrough/auto");
    await user.click(screen.getByTestId("provider-save"));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "my gw",
        kind: "gateway",
        baseUrl: "http://127.0.0.1:1",
        authToken: "sk-dummy-token-zzzz",
        defaultGateway: true,
        models: ["passthrough/auto"],
      }),
    );
    expect(screen.getByTestId("provider-name")).not.toHaveValue("sk-dummy-token-zzzz");
  });
});
