import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Instance } from "../../../types/instance";
import { PhoneKeyBar } from "./PhoneKeyBar";

const navigate = vi.fn();
const openQuickFind = vi.fn();
const probeClipboardRead = vi.fn();
const readClipboard = vi.fn();
const onKey = vi.fn();
const onFillInput = vi.fn();
const toast = vi.fn();

vi.mock("react-router-dom", async () => {
  const actual = await vi.importActual<typeof import("react-router-dom")>("react-router-dom");
  return { ...actual, useNavigate: () => navigate };
});
vi.mock("../../../lib/store", () => ({
  useHub: () => ({ events: {} }),
  hubStore: { toast: (...args: unknown[]) => toast(...args) },
}));
vi.mock("../../search/QuickFind", () => ({
  openQuickFind: (...args: unknown[]) => openQuickFind(...args),
}));
vi.mock("../../../lib/clipboard", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../../lib/clipboard")>();
  return {
    ...actual,
    probeClipboardRead: (...args: unknown[]) => probeClipboardRead(...args),
    readClipboard: (...args: unknown[]) => readClipboard(...args),
  };
});
vi.mock("./promptHistory", () => ({
  promptHistory: () => ["git status", "ls -la"],
}));

const instance = { id: "ins_keybar" } as Instance;

const captureScrollLine = vi.fn(() => 3);

function renderBar(disabled = false) {
  return render(
    <MemoryRouter>
      <PhoneKeyBar
        instance={instance}
        disabled={disabled}
        onKey={onKey}
        captureScrollLine={captureScrollLine}
        onFillInput={onFillInput}
      />
    </MemoryRouter>,
  );
}

/** The nine-key table, left to right per mobile-ui plan B.3.2. */
const NINE = [
  ["ctrl", "Ctrl"],
  ["esc", "Esc"],
  ["tab", "Tab"],
  ["git", "git"],
  ["jump", "跳转"],
  ["clip", "贴"],
  ["history", "史"],
  ["view", "结构"],
  ["keyboard", "键"],
] as const;

beforeEach(() => {
  vi.clearAllMocks();
  probeClipboardRead.mockResolvedValue({ state: "ready" });
});

describe("PhoneKeyBar nine-key table (B.3.2)", () => {
  it("renders exactly the nine keys in order with the right labels", async () => {
    renderBar();
    await waitFor(() => expect(screen.getByTestId("phone-key-git")).toBeEnabled());
    const keys = screen.getAllByTestId(/^phone-key-/);
    expect(keys.map((key) => key.dataset.testid)).toEqual(NINE.map(([id]) => `phone-key-${id}`));
    for (const [id, label] of NINE) {
      expect(screen.getByTestId(`phone-key-${id}`)).toHaveTextContent(label);
    }
  });
});

describe("sticky Ctrl", () => {
  it("sticks until the next byte key and is one-shot", async () => {
    const user = userEvent.setup();
    renderBar();
    const ctrl = screen.getByTestId("phone-key-ctrl");
    await waitFor(() => expect(ctrl).toBeEnabled());
    expect(ctrl).toHaveAttribute("aria-pressed", "false");
    await user.click(ctrl);
    expect(ctrl).toHaveAttribute("aria-pressed", "true");
    await user.click(screen.getByTestId("phone-key-esc"));
    expect(onKey).toHaveBeenCalledWith("");
    expect(ctrl).toHaveAttribute("aria-pressed", "false");
  });

  it("is shared with the expanded raw-key second row", async () => {
    const user = userEvent.setup();
    renderBar();
    await user.click(screen.getByTestId("phone-key-ctrl"));
    await user.click(screen.getByTestId("phone-key-keyboard"));
    // The full BAR set returns — alt and ⌃C are not capabilities the phone
    // layout is allowed to lose (acceptance 7).
    expect(screen.getByTestId("tty-key-alt")).toBeVisible();
    expect(screen.getByTestId("tty-key-ctrl-c")).toBeVisible();
    expect(screen.getByTestId("tty-key-pgup")).toBeVisible();
    expect(screen.getByTestId("tty-key-pgdn")).toBeVisible();
    const rowCtrl = screen.getByTestId("tty-key-ctrl");
    expect(rowCtrl).toHaveAttribute("aria-pressed", "true");
    await user.click(rowCtrl);
    expect(screen.getByTestId("phone-key-ctrl")).toHaveAttribute("aria-pressed", "false");
    await user.click(screen.getByTestId("tty-key-ctrl-c"));
    expect(onKey).toHaveBeenCalledWith("\u0003");
  });
});

describe("history key (D-028a write boundary)", () => {
  it("fills the chosen prompt into the local strip and never sends", async () => {
    const user = userEvent.setup();
    renderBar();
    await user.click(screen.getByTestId("phone-key-history"));
    expect(screen.getByTestId("phone-history-sheet")).toBeVisible();
    const items = screen.getAllByTestId("phone-history-item");
    expect(items.map((item) => item.textContent)).toEqual(["git status", "ls -la"]);
    await user.click(items[0]!);
    expect(onFillInput).toHaveBeenCalledTimes(1);
    expect(onFillInput).toHaveBeenCalledWith("git status");
    expect(onKey).not.toHaveBeenCalled();
    expect(screen.queryByTestId("phone-history-sheet")).toBeNull();
  });
});

describe("clipboard key", () => {
  it("is disabled with a visible reason while the permission/API is unavailable, and never acts", async () => {
    probeClipboardRead.mockResolvedValue({
      state: "blocked",
      reason: "浏览器拒绝了剪贴板读取权限",
    });
    const user = userEvent.setup();
    renderBar();
    const clip = await screen.findByTestId("phone-key-clip");
    expect(clip).toBeDisabled();
    expect(screen.getByTestId("phone-clip-reason")).toHaveTextContent("浏览器拒绝了剪贴板读取权限");
    await user.click(clip);
    expect(readClipboard).not.toHaveBeenCalled();
    expect(onKey).not.toHaveBeenCalled();
  });

  it("pastes clipboard text onto the raw tty channel when permitted", async () => {
    readClipboard.mockResolvedValue("cargo test\r");
    const user = userEvent.setup();
    renderBar();
    const clip = await screen.findByTestId("phone-key-clip");
    await waitFor(() => expect(clip).toBeEnabled());
    await user.click(clip);
    await waitFor(() => expect(onKey).toHaveBeenCalledWith("cargo test\r"));
  });

  it("surfaces a denial at read time instead of failing silently", async () => {
    readClipboard.mockRejectedValue(new Error("denied"));
    const user = userEvent.setup();
    renderBar();
    const clip = await screen.findByTestId("phone-key-clip");
    await waitFor(() => expect(clip).toBeEnabled());
    await user.click(clip);
    await waitFor(() => expect(toast).toHaveBeenCalled());
    expect(clip).toBeDisabled();
    expect(screen.getByTestId("phone-clip-reason")).toBeVisible();
  });
});

describe("navigation keys", () => {
  it("git captures the terminal scroll line then opens the existing files route", async () => {
    const user = userEvent.setup();
    renderBar();
    await user.click(screen.getByTestId("phone-key-git"));
    expect(captureScrollLine).toHaveBeenCalledTimes(1);
    expect(navigate).toHaveBeenCalledWith("/s/ins_keybar/files");
  });

  it("Jump To goes through the exported openQuickFind (degraded path until task 7)", async () => {
    const user = userEvent.setup();
    renderBar();
    await user.click(screen.getByTestId("phone-key-jump"));
    expect(openQuickFind).toHaveBeenCalledTimes(1);
  });

  it("the view key navigates the same /s/:id/<view> path as the top-bar ViewSwitch", async () => {
    const user = userEvent.setup();
    renderBar();
    await user.click(screen.getByTestId("phone-key-view"));
    expect(navigate).toHaveBeenCalledWith("/s/ins_keybar/structured");
  });
});

describe("frozen gate", () => {
  it("disables byte-writing keys on a stale frame", async () => {
    renderBar(true);
    await waitFor(() =>
      expect(screen.getByTestId("phone-key-clip")).toBeDisabled(),
    );
    expect(screen.getByTestId("phone-key-esc")).toBeDisabled();
    expect(screen.getByTestId("phone-key-tab")).toBeDisabled();
    // Navigation never writes bytes: git / view stay usable while frozen.
    expect(screen.getByTestId("phone-key-git")).toBeEnabled();
    expect(screen.getByTestId("phone-key-view")).toBeEnabled();
  });
});
