import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ModelList } from "./ModelList";
import type { ProviderModel } from "./model";

/** A catalog shaped like a real gateway's: several prefixes, mixed surfaces. */
const catalog: ProviderModel[] = [
  { id: "passthrough/auto", enabled: true, label: "Auto", surfaces: ["openai"] },
  { id: "passthrough/ark/seed-evolving", enabled: true, surfaces: ["openai"] },
  { id: "cursor/gpt-5", enabled: false, surfaces: ["openai"] },
  { id: "claude-opus-5", enabled: true, label: "Opus 5", contextWindow: 1_048_576, tags: ["1m"], surfaces: ["openai", "anthropic"] },
  { id: "claude-haiku-4-5", enabled: true, surfaces: ["anthropic"] },
];

function ids() {
  return screen.queryAllByTestId("provider-model-row").map((row) => row.getAttribute("data-model"));
}

function renderList(models = catalog) {
  const onChange = vi.fn();
  render(
    <ModelList
      models={models}
      defaultModel="passthrough/auto"
      onChange={onChange}
      onDefaultChange={() => undefined}
    />,
  );
  return { onChange };
}

describe("ModelList", () => {
  it("groups by id prefix and counts the enabled models per group", () => {
    renderList();
    const groups = screen.getAllByTestId("provider-model-group");
    expect(groups.map((g) => g.getAttribute("data-group"))).toEqual([
      "passthrough/",
      "cursor/",
      "claude-",
    ]);
    // 0 of 1 enabled in cursor/, 2 of 2 in claude-.
    expect(within(groups[1]).getByTestId("provider-model-group-toggle")).toHaveTextContent("0/1");
    expect(within(groups[2]).getByTestId("provider-model-group-toggle")).toHaveTextContent("2/2");
    expect(ids()).toHaveLength(catalog.length);
  });

  it("collapses and reopens one group without touching the others", async () => {
    const user = userEvent.setup();
    renderList();
    const passthrough = screen.getAllByTestId("provider-model-group")[0];
    await user.click(within(passthrough).getByTestId("provider-model-group-toggle"));

    expect(passthrough).toHaveAttribute("data-collapsed", "1");
    expect(ids()).toEqual(["cursor/gpt-5", "claude-opus-5", "claude-haiku-4-5"]);

    await user.click(within(passthrough).getByTestId("provider-model-group-toggle"));
    expect(ids()).toHaveLength(catalog.length);
  });

  it("filters on id or label and reports when nothing matches", async () => {
    const user = userEvent.setup();
    renderList();
    const search = screen.getByTestId("provider-model-search");

    await user.type(search, "claude");
    expect(ids()).toEqual(["claude-opus-5", "claude-haiku-4-5"]);

    await user.clear(search);
    await user.type(search, "Opus");
    expect(ids()).toEqual(["claude-opus-5"]);

    await user.clear(search);
    await user.type(search, "zzz");
    expect(screen.getByTestId("provider-models-no-match")).toBeVisible();
    expect(ids()).toHaveLength(0);

    // Clearing the box restores the full catalog.
    await user.clear(search);
    expect(ids()).toHaveLength(catalog.length);
  });

  it("shows which gateway listing reported each model", () => {
    renderList();
    const shared = screen.getAllByTestId("provider-model-row").find(
      (row) => row.getAttribute("data-model") === "claude-opus-5",
    )!;
    expect(
      within(shared)
        .getAllByTestId("provider-model-surface")
        .map((chip) => chip.textContent),
    ).toEqual(["openai", "anthropic"]);
    const anthropicOnly = screen.getAllByTestId("provider-model-row").find(
      (row) => row.getAttribute("data-model") === "claude-haiku-4-5",
    )!;
    expect(
      within(anthropicOnly)
        .getAllByTestId("provider-model-surface")
        .map((chip) => chip.textContent),
    ).toEqual(["anthropic"]);
  });

  it("toggling a filtered model still edits the full catalog, not the view", async () => {
    const user = userEvent.setup();
    const { onChange } = renderList();
    await user.type(screen.getByTestId("provider-model-search"), "haiku");
    await user.click(screen.getByTestId("provider-model-enabled"));
    // Every model survives the edit; only the filtered one flipped.
    expect(onChange).toHaveBeenCalledWith(
      catalog.map((m) => (m.id === "claude-haiku-4-5" ? { ...m, enabled: false } : m)),
    );
  });

  it("keeps the manual chip input for a gateway that lists nothing", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(
      <ModelList
        models={[]}
        defaultModel=""
        onChange={onChange}
        onDefaultChange={() => undefined}
      />,
    );
    expect(screen.getByTestId("provider-models-empty")).toBeVisible();
    // No catalog means no search box to get in the way.
    expect(screen.queryByTestId("provider-model-search")).toBeNull();
    await user.type(screen.getByTestId("provider-model-manual"), "typed/id");
    await user.click(screen.getByTestId("provider-model-add"));
    expect(onChange).toHaveBeenCalledWith([{ id: "typed/id", enabled: true }]);
  });
});
