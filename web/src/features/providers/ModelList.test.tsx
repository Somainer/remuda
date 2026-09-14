import { useState } from "react";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
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

/**
 * The list is controlled, so bulk edits only read correctly against a parent
 * that actually holds the state — the same wiring ProviderForm uses.
 */
function Harness({
  initial,
  initialDefault = "passthrough/auto",
  profileId,
  onState,
}: {
  initial: ProviderModel[];
  initialDefault?: string;
  profileId?: string;
  onState?: (models: ProviderModel[], defaultModel: string) => void;
}) {
  const [models, setModels] = useState(initial);
  const [defaultModel, setDefaultModel] = useState(initialDefault);
  onState?.(models, defaultModel);
  return (
    <ModelList
      models={models}
      defaultModel={defaultModel}
      profileId={profileId}
      onChange={setModels}
      onDefaultChange={setDefaultModel}
    />
  );
}

function renderStateful(props: Parameters<typeof Harness>[0]) {
  const state = { models: props.initial, defaultModel: props.initialDefault ?? "passthrough/auto" };
  render(
    <Harness
      {...props}
      onState={(models, defaultModel) => {
        state.models = models;
        state.defaultModel = defaultModel;
      }}
    />,
  );
  return state;
}

function group(key: string) {
  return screen.getAllByTestId("provider-model-group").find((g) => g.getAttribute("data-group") === key)!;
}

function enabledIds(models: ProviderModel[]) {
  return models.filter((m) => m.enabled).map((m) => m.id);
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

describe("ModelList bulk controls", () => {
  beforeEach(() => localStorage.clear());

  it("select-all names the scope and only touches the filtered rows", async () => {
    const user = userEvent.setup();
    const state = renderStateful({ initial: catalog });
    // Unfiltered, the label counts the whole catalog.
    expect(screen.getByTestId("provider-models-all")).toHaveTextContent("全选（全部 5）");

    await user.type(screen.getByTestId("provider-model-search"), "cursor");
    expect(screen.getByTestId("provider-models-all")).toHaveTextContent("全选（筛选结果 1）");
    await user.click(screen.getByTestId("provider-models-all"));
    // cursor/gpt-5 was the only unticked model, and the only one in view.
    expect(enabledIds(state.models)).toHaveLength(5);

    // 全不选 with the same filter hides only the row on screen.
    await user.click(screen.getByTestId("provider-models-none"));
    expect(enabledIds(state.models)).toEqual([
      "passthrough/auto",
      "passthrough/ark/seed-evolving",
      "claude-opus-5",
      "claude-haiku-4-5",
    ]);
  });

  it("全不选 over the whole catalog clears the count, and 反选 flips it back", async () => {
    const user = userEvent.setup();
    const state = renderStateful({ initial: catalog });
    await user.click(screen.getByTestId("provider-models-none"));
    expect(enabledIds(state.models)).toEqual([]);
    expect(screen.getByTestId("provider-models-count")).toHaveTextContent("0/5 已启用");

    await user.click(screen.getByTestId("provider-models-invert"));
    expect(enabledIds(state.models)).toHaveLength(5);
    expect(screen.getByTestId("provider-models-count")).toHaveTextContent("5/5 已启用");
  });

  it("undoes one bulk action, including the default it moved", async () => {
    const user = userEvent.setup();
    const state = renderStateful({ initial: catalog });
    await user.click(screen.getByTestId("provider-models-none"));
    expect(state.defaultModel).toBe("");
    const notice = screen.getByTestId("provider-models-undo");
    expect(notice).toHaveTextContent("已停用 5 个模型");

    await user.click(screen.getByTestId("provider-models-undo-button"));
    expect(enabledIds(state.models)).toEqual([
      "passthrough/auto",
      "passthrough/ark/seed-evolving",
      "claude-opus-5",
      "claude-haiku-4-5",
    ]);
    expect(state.defaultModel).toBe("passthrough/auto");
    expect(screen.queryByTestId("provider-models-undo")).toBeNull();
  });

  it("moves the default off a model a bulk disable hid, and says so", async () => {
    const user = userEvent.setup();
    const state = renderStateful({ initial: catalog });
    await user.type(screen.getByTestId("provider-model-search"), "passthrough");
    await user.click(screen.getByTestId("provider-models-none"));
    // passthrough/auto was the default; the first still-enabled model takes it.
    expect(state.defaultModel).toBe("claude-opus-5");
    expect(screen.getByTestId("provider-models-undo")).toHaveTextContent("默认模型改为 claude-opus-5");
  });

  it("disables a bulk button that would change nothing", async () => {
    const user = userEvent.setup();
    renderStateful({ initial: catalog });
    await user.click(screen.getByTestId("provider-models-all"));
    expect(screen.getByTestId("provider-models-all")).toBeDisabled();
    expect(screen.getByTestId("provider-models-none")).toBeEnabled();
    // Nothing in view means nothing to act on.
    await user.type(screen.getByTestId("provider-model-search"), "zzz");
    expect(screen.getByTestId("provider-models-invert")).toBeDisabled();
  });
});

describe("ModelList group controls", () => {
  beforeEach(() => localStorage.clear());

  it("renders each group's tri-state and enabled count", () => {
    renderStateful({ initial: catalog });
    // 2/2 on, 0/1 on, 2/2 on.
    expect(group("passthrough/")).toHaveAttribute("data-state", "all");
    expect(group("cursor/")).toHaveAttribute("data-state", "none");
    expect(within(group("claude-")).getByTestId("provider-model-group-count")).toHaveTextContent(
      "2/2 已启用",
    );
    const some = screen.getAllByTestId("provider-model-group-enabled");
    expect(some[0]).toBeChecked();
    expect(some[1]).not.toBeChecked();
  });

  it("shows the indeterminate box once a group is partly enabled", async () => {
    const user = userEvent.setup();
    renderStateful({ initial: catalog });
    const rows = within(group("passthrough/")).getAllByTestId("provider-model-enabled");
    await user.click(rows[0]);
    expect(group("passthrough/")).toHaveAttribute("data-state", "some");
    const box = within(group("passthrough/")).getByTestId("provider-model-group-enabled");
    expect(box).not.toBeChecked();
    expect((box as HTMLInputElement).indeterminate).toBe(true);
  });

  it("the group box toggles the whole group and nothing else", async () => {
    const user = userEvent.setup();
    const state = renderStateful({ initial: catalog });
    await user.click(within(group("claude-")).getByTestId("provider-model-group-enabled"));
    expect(enabledIds(state.models)).toEqual(["passthrough/auto", "passthrough/ark/seed-evolving"]);
    expect(screen.getByTestId("provider-models-undo")).toHaveTextContent("已停用 claude- 的 2 个模型");

    // Ticking a "none" group turns all of it on.
    await user.click(within(group("cursor/")).getByTestId("provider-model-group-enabled"));
    expect(enabledIds(state.models)).toContain("cursor/gpt-5");
  });

  it("a filtered group toggles only the rows that match", async () => {
    const user = userEvent.setup();
    const wide: ProviderModel[] = [
      { id: "cursor/gpt-5", enabled: false },
      { id: "cursor/gpt-5-mini", enabled: false },
      { id: "cursor/sonic", enabled: false },
    ];
    const state = renderStateful({ initial: wide, initialDefault: "" });
    await user.type(screen.getByTestId("provider-model-search"), "gpt");
    // The count is the filtered one, so the header never claims rows off screen.
    expect(within(group("cursor/")).getByTestId("provider-model-group-count")).toHaveTextContent(
      "0/2 已启用",
    );
    await user.click(within(group("cursor/")).getByTestId("provider-model-group-enabled"));
    expect(enabledIds(state.models)).toEqual(["cursor/gpt-5", "cursor/gpt-5-mini"]);
    // The group reads "all" against what is shown, though sonic stays off.
    expect(group("cursor/")).toHaveAttribute("data-state", "all");
  });

  it("renders a one-model group as a plain row, with no header", () => {
    renderStateful({ initial: [{ id: "solo", enabled: true }, ...catalog], initialDefault: "solo" });
    expect(group("solo")).toHaveAttribute("data-solo", "1");
    expect(within(group("solo")).queryByTestId("provider-model-group-toggle")).toBeNull();
    expect(within(group("solo")).getByTestId("provider-model-row")).toBeVisible();
    // A prefix group of one is still a group: cursor/ keeps its header.
    expect(group("cursor/")).toHaveAttribute("data-solo", "0");
    expect(within(group("cursor/")).getByTestId("provider-model-group-toggle")).toBeVisible();
  });

  it("persists collapse per profile and restores it on the next mount", async () => {
    const user = userEvent.setup();
    const { unmount } = render(<Harness initial={catalog} profileId="pvp_a" />);
    await user.click(within(group("cursor/")).getByTestId("provider-model-group-toggle"));
    expect(group("cursor/")).toHaveAttribute("data-collapsed", "1");
    unmount();

    const second = render(<Harness initial={catalog} profileId="pvp_a" />);
    expect(group("cursor/")).toHaveAttribute("data-collapsed", "1");
    expect(group("passthrough/")).toHaveAttribute("data-collapsed", "0");
    // A collapsed group hides its rows but still reports its count.
    expect(ids()).toEqual([
      "passthrough/auto",
      "passthrough/ark/seed-evolving",
      "claude-opus-5",
      "claude-haiku-4-5",
    ]);
    expect(within(group("cursor/")).getByTestId("provider-model-group-count")).toHaveTextContent(
      "0/1 已启用",
    );
    second.unmount();

    // Another profile starts fully expanded.
    render(<Harness initial={catalog} profileId="pvp_b" />);
    expect(group("cursor/")).toHaveAttribute("data-collapsed", "0");
  });
});
