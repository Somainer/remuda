import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ProviderForm } from "./ProviderForm";
import type { ProviderProfile } from "./model";

describe("ProviderForm", () => {
  it("submits a gateway profile without echoing the token in the name field", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(<ProviderForm onSubmit={onSubmit} onCancel={() => undefined} />);
    await user.type(screen.getByTestId("provider-name"), "my gw");
    await user.type(screen.getByTestId("provider-base-url"), "http://127.0.0.1:1");
    await user.type(screen.getByTestId("provider-token"), "sk-dummy-token-zzzz");
    await user.type(screen.getByTestId("provider-model-manual"), "passthrough/auto");
    await user.click(screen.getByTestId("provider-model-add"));
    await user.click(screen.getByTestId("provider-save"));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "my gw",
        kind: "gateway",
        baseUrl: "http://127.0.0.1:1",
        authToken: "sk-dummy-token-zzzz",
        defaultGateway: true,
        models: [{ id: "passthrough/auto", enabled: true }],
        defaultModel: "passthrough/auto",
        scope: "universal",
      }),
    );
    expect(screen.getByTestId("provider-name")).not.toHaveValue("sk-dummy-token-zzzz");
  });

  it("submits a host-scoped profile", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(
      <ProviderForm
        hosts={[{ id: "hst_a", label: "box-a" }]}
        onSubmit={onSubmit}
        onCancel={() => undefined}
      />,
    );
    await user.type(screen.getByTestId("provider-name"), "host gw");
    await user.type(screen.getByTestId("provider-base-url"), "http://127.0.0.1:1");
    await user.type(screen.getByTestId("provider-token"), "sk-dummy-token-host");
    await user.click(screen.getByTestId("provider-scope-host"));
    expect(screen.getByTestId("provider-scope-host-id")).toHaveValue("hst_a");
    await user.click(screen.getByTestId("provider-save"));
    expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ scope: "host:hst_a", name: "host gw" }));
  });

  it("discovers models, exposes only the ticked ones, and sends the token once", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const onDiscover = vi.fn().mockResolvedValue([
      { id: "gw/a", enabled: true, label: "A", contextWindow: 1_048_576, tags: ["1m"] },
      { id: "gw/b", enabled: true },
    ]);
    render(<ProviderForm onDiscover={onDiscover} onSubmit={onSubmit} onCancel={() => undefined} />);
    await user.type(screen.getByTestId("provider-name"), "probe gw");
    await user.type(screen.getByTestId("provider-base-url"), "https://gw.example/v1");
    await user.type(screen.getByTestId("provider-token"), "sk-fake-probe-pppp");
    await user.click(screen.getByTestId("provider-discover"));

    await waitFor(() => expect(screen.getAllByTestId("provider-model-row")).toHaveLength(2));
    expect(onDiscover).toHaveBeenCalledWith({
      baseUrl: "https://gw.example/v1",
      token: "sk-fake-probe-pppp",
    });
    const rows = screen.getAllByTestId("provider-model-row");
    expect(rows[0]).toHaveAttribute("data-model", "gw/a");
    expect(within(rows[0]).getByTestId("provider-model-tag")).toHaveTextContent("1m");
    expect(screen.getByTestId("provider-models-count")).toHaveTextContent("2/2 已启用");

    // Untick the second model: it stays listed but must not be exposed.
    await user.click(within(rows[1]).getByTestId("provider-model-enabled"));
    expect(screen.getByTestId("provider-models-count")).toHaveTextContent("1/2 已启用");
    await user.click(screen.getByTestId("provider-save"));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        models: [
          { id: "gw/a", enabled: true, label: "A", contextWindow: 1_048_576, tags: ["1m"] },
          { id: "gw/b", enabled: false },
        ],
        defaultModel: "gw/a",
      }),
    );
  });

  it("shows a discovery failure inline and leaves the list alone", async () => {
    const user = userEvent.setup();
    const onDiscover = vi.fn().mockRejectedValue(new Error("unreachable: connection refused"));
    render(<ProviderForm onDiscover={onDiscover} onSubmit={vi.fn()} onCancel={() => undefined} />);
    await user.type(screen.getByTestId("provider-base-url"), "http://127.0.0.1:1");
    await user.click(screen.getByTestId("provider-discover"));
    await waitFor(() =>
      expect(screen.getByTestId("provider-discover-error")).toHaveTextContent("unreachable"),
    );
    expect(screen.queryAllByTestId("provider-model-row")).toHaveLength(0);
    expect(screen.getByTestId("provider-models-empty")).toBeVisible();
  });

  it("pre-checks saved models in edit mode and flags discovered-but-new ones", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const initial: ProviderProfile = {
      id: "pvp_edit",
      profileId: "pvp_edit",
      name: "saved gw",
      delegation: "gateway",
      kind: "gateway",
      protocol: "anthropic-messages",
      baseUrl: "https://gw.example/v1",
      health: null,
      secret: { present: true, last4: "zzzz", fingerprint: "0123456789abcdef" },
      secretRef: "zzzz",
      models: [
        { id: "gw/saved", enabled: true },
        { id: "gw/hidden", enabled: false },
      ],
      defaultModel: "gw/saved",
      defaultGateway: true,
      scope: "universal",
      headers: {},
      lastError: null,
      rotationOwner: "gateway",
      available: true,
    };
    const onDiscover = vi.fn().mockResolvedValue([
      { id: "gw/saved", enabled: true, label: "Saved" },
      { id: "gw/hidden", enabled: true },
      { id: "gw/brand-new", enabled: true },
    ]);
    render(
      <ProviderForm initial={initial} onDiscover={onDiscover} onSubmit={onSubmit} onCancel={() => undefined} />,
    );
    // The saved catalog is reflected before any probe.
    const before = screen.getAllByTestId("provider-model-row");
    expect(before).toHaveLength(2);
    expect(within(before[0]).getByTestId("provider-model-enabled")).toBeChecked();
    expect(within(before[1]).getByTestId("provider-model-enabled")).not.toBeChecked();
    expect(within(before[0]).getByTestId("provider-model-default")).toBeChecked();

    await user.click(screen.getByTestId("provider-discover"));
    await waitFor(() => expect(screen.getAllByTestId("provider-model-row")).toHaveLength(3));
    // Editing without retyping the token re-probes with the stored one.
    expect(onDiscover).toHaveBeenCalledWith({
      baseUrl: "https://gw.example/v1",
      token: "",
      profileId: "pvp_edit",
    });
    const rows = screen.getAllByTestId("provider-model-row");
    // A model the operator had unticked stays unticked across a re-probe.
    expect(within(rows[1]).getByTestId("provider-model-enabled")).not.toBeChecked();
    expect(rows[1]).toHaveAttribute("data-new", "0");
    expect(rows[2]).toHaveAttribute("data-new", "1");
    expect(within(rows[2]).getByTestId("provider-model-new")).toBeVisible();
  });

  it("adds a manual id for gateways that list nothing and retargets the default", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(<ProviderForm onSubmit={onSubmit} onCancel={() => undefined} />);
    await user.type(screen.getByTestId("provider-name"), "manual gw");
    await user.type(screen.getByTestId("provider-base-url"), "https://gw.example/v1");
    await user.type(screen.getByTestId("provider-token"), "sk-fake-manual-mmmm");
    const manual = screen.getByTestId("provider-model-manual");
    // Enter adds a chip rather than submitting the form.
    await user.type(manual, "gw/one{Enter}");
    expect(onSubmit).not.toHaveBeenCalled();
    await user.type(manual, "gw/two{Enter}");
    expect(screen.getAllByTestId("provider-model-row")).toHaveLength(2);
    expect(manual).toHaveValue("");

    // The first manual id becomes the default; picking the second moves it.
    const rows = screen.getAllByTestId("provider-model-row");
    expect(within(rows[0]).getByTestId("provider-model-default")).toBeChecked();
    await user.click(within(rows[1]).getByTestId("provider-model-default"));
    await user.click(screen.getByTestId("provider-save"));
    expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ defaultModel: "gw/two" }));
  });

  it("drops a removed model and never leaves a disabled default", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(<ProviderForm onSubmit={onSubmit} onCancel={() => undefined} />);
    await user.type(screen.getByTestId("provider-name"), "gw");
    await user.type(screen.getByTestId("provider-base-url"), "https://gw.example/v1");
    await user.type(screen.getByTestId("provider-token"), "sk-fake-dddd");
    await user.type(screen.getByTestId("provider-model-manual"), "gw/one{Enter}");
    await user.type(screen.getByTestId("provider-model-manual"), "gw/two{Enter}");

    // Unticking the default hands it to the next enabled model.
    const rows = screen.getAllByTestId("provider-model-row");
    await user.click(within(rows[0]).getByTestId("provider-model-enabled"));
    expect(within(rows[0]).getByTestId("provider-model-default")).toBeDisabled();
    expect(within(rows[1]).getByTestId("provider-model-default")).toBeChecked();

    await user.click(within(rows[1]).getByTestId("provider-model-remove"));
    expect(screen.getAllByTestId("provider-model-row")).toHaveLength(1);
    await user.click(screen.getByTestId("provider-save"));
    // gw/one is still listed but disabled, so nothing enabled remains.
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ models: [{ id: "gw/one", enabled: false }], defaultModel: undefined }),
    );
  });
});
