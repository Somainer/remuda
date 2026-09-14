import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { readDeviceSettings } from "../features/settings";
import { SettingsPage } from "./SettingsPage";

const pushMock = vi.hoisted(() => ({
  readPushStatus: vi.fn(),
  subscribePush: vi.fn(),
  unsubscribePush: vi.fn(),
}));

vi.mock("../lib/push", () => pushMock);

function renderSettings(initialEntries: string[]) {
  return render(
    <MemoryRouter initialEntries={initialEntries}>
      <Routes>
        <Route path="/sessions" element={<div data-testid="sessions-marker">会话列表</div>} />
        <Route path="/settings" element={<SettingsPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

const baseHub = {
  ...store.hubStore.getSnapshot(),
  devices: [],
  passkeys: [],
  compact: true,
  pairCode: null,
};

beforeEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("data-theme");
  vi.spyOn(store, "useHub").mockReturnValue(baseHub);
  vi.spyOn(store.hubStore, "passkeysSupported").mockReturnValue(true);
  vi.spyOn(store.hubStore, "setCompact").mockImplementation(() => {});
  pushMock.readPushStatus.mockResolvedValue({
    permission: "default",
    subscribed: false,
    endpoint: null,
    needsHomeScreen: false,
  });
  pushMock.subscribePush.mockResolvedValue(undefined);
  pushMock.unsubscribePush.mockResolvedValue(undefined);
});

afterEach(() => vi.restoreAllMocks());

describe("grouping and anchors", () => {
  it("renders the three anchored groups with in-page navigation", () => {
    renderSettings(["/settings"]);
    expect(screen.getByTestId("settings-nav")).toBeInTheDocument();
    for (const id of ["appearance", "notifications", "connection"]) {
      const link = screen.getByTestId(`settings-nav-${id}`);
      expect(link).toHaveAttribute("href", `/settings#${id}`);
      expect(screen.getByTestId(`settings-group-${id}`)).toBeInTheDocument();
    }
  });

  it("marks the deep-linked group current and focuses its section", async () => {
    renderSettings(["/settings#notifications"]);
    await waitFor(() =>
      expect(screen.getByTestId("settings-nav-notifications")).toHaveAttribute("aria-current", "true"),
    );
    expect(screen.getByTestId("settings-nav-appearance")).not.toHaveAttribute("aria-current");
    expect(screen.getByTestId("settings-nav-connection")).not.toHaveAttribute("aria-current");
    await waitFor(() => expect(document.activeElement).toBe(screen.getByTestId("settings-group-notifications")));
  });

  it("back from a deep link with no history falls back to the sessions list", async () => {
    renderSettings(["/settings#connection"]);
    fireEvent.click(screen.getByTestId("settings-back"));
    expect(screen.getByTestId("sessions-marker")).toBeInTheDocument();
  });

  it("back returns to the originating route when history exists", () => {
    render(
      <MemoryRouter initialEntries={["/sessions", "/settings"]} initialIndex={1}>
        <Routes>
          <Route path="/sessions" element={<div data-testid="sessions-marker">会话列表</div>} />
          <Route path="/settings" element={<SettingsPage />} />
        </Routes>
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByTestId("settings-back"));
    expect(screen.getByTestId("sessions-marker")).toBeInTheDocument();
  });
});

describe("no new default permissions", () => {
  it("ships the same defaults: manual permission, autoReveal off, high effort, night", () => {
    renderSettings(["/settings"]);
    expect(screen.getByTestId("settings-perm-manual")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("settings-perm-acceptEdits")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("settings-perm-dontAsk")).toHaveAttribute("aria-pressed", "false");
    // bypassPermissions exists in the storage type but must never be offered.
    expect(screen.queryByTestId("settings-perm-bypassPermissions")).toBeNull();
    expect(screen.getByTestId("settings-auto-reveal-tty")).not.toBeChecked();
    expect(screen.getByTestId("settings-effort-high")).toHaveAttribute("data-selected", "1");
    expect(screen.getByTestId("settings-theme-night")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("settings-device-name")).toHaveValue("this-device");
  });
});

const originalSetItem = Storage.prototype.setItem;

describe("save states and rollback", () => {
  it("shows 保存中 then 已保存 for a theme change and persists it immediately", async () => {
    renderSettings(["/settings"]);
    // Local appearance prefs commit on selection — no draft save button.
    fireEvent.click(screen.getByTestId("settings-theme-ledger"));

    // The saving state is rendered before the persist settles.
    expect(screen.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "saving");
    expect(screen.getByTestId("settings-appearance-status")).toHaveTextContent("保存中");

    await waitFor(() =>
      expect(screen.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "saved"),
    );
    const saved = screen.getByTestId("settings-appearance-status");
    expect(saved).toHaveTextContent("已保存");
    expect(localStorage.getItem("runtime.theme.v1")).toBe("ledger");
    expect(document.documentElement.dataset.theme).toBe("ledger");
  });

  it("marks a failed theme save, rolls that field back, and keeps the error visible", async () => {
    const setItem = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(function (this: Storage, key: string, value: string) {
        if (key === "runtime.theme.v1") throw new Error("quota");
        originalSetItem.call(this, key, value);
      });
    renderSettings(["/settings"]);
    fireEvent.click(screen.getByTestId("settings-theme-ledger"));
    await waitFor(() =>
      expect(screen.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "error"),
    );
    const failed = screen.getByTestId("settings-appearance-status");
    expect(failed).toHaveTextContent("失败");
    // The rejected field is back at its last committed value…
    await waitFor(() => expect(screen.getByTestId("settings-theme-night")).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getByTestId("settings-theme-ledger")).toHaveAttribute("aria-pressed", "false");
    expect(document.documentElement.dataset.theme).toBe("night");
    // …and the failure is not covered by a later "saved".
    expect(failed).toHaveTextContent("失败");
    setItem.mockRestore();
  });

  it("keeps a committed permission choice even when a later theme change rejects", async () => {
    const setItem = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(function (this: Storage, key: string, value: string) {
        if (key === "runtime.theme.v1") throw new Error("quota");
        originalSetItem.call(this, key, value);
      });
    renderSettings(["/settings"]);
    // The permission pref is a separate immediate commit and lands first.
    fireEvent.click(screen.getByTestId("settings-perm-acceptEdits"));
    await waitFor(() =>
      expect(screen.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "saved"),
    );
    expect(readDeviceSettings().permissionDefault).toBe("acceptEdits");
    // The later theme change fails and rolls only the theme back.
    fireEvent.click(screen.getByTestId("settings-theme-ledger"));
    await waitFor(() =>
      expect(screen.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "error"),
    );
    expect(screen.getByTestId("settings-perm-acceptEdits")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("settings-theme-night")).toHaveAttribute("aria-pressed", "true");
    expect(readDeviceSettings().permissionDefault).toBe("acceptEdits");
    setItem.mockRestore();
  });

  it("rolls an invalid device name back to the last valid value", async () => {
    renderSettings(["/settings"]);
    const name = screen.getByTestId("settings-device-name");
    // Whitespace-only is never a valid name; the save rejects just this field.
    fireEvent.change(name, { target: { value: "   " } });
    fireEvent.click(screen.getByTestId("settings-identity-save"));
    await waitFor(() =>
      expect(screen.getByTestId("settings-identity-status")).toHaveAttribute("data-phase", "error"),
    );
    const failed = screen.getByTestId("settings-identity-status");
    expect(failed).toHaveTextContent("设备名不能为空");
    await waitFor(() => expect(name).toHaveValue("this-device"));
    // The rejected value never reached storage; the reader still yields default.
    expect(readDeviceSettings().deviceName).toBe("this-device");
  });

  it("a successful identity save stores device name and access code", async () => {
    renderSettings(["/settings"]);
    fireEvent.change(screen.getByTestId("settings-device-name"), { target: { value: "desk-2" } });
    fireEvent.click(screen.getByTestId("settings-identity-save"));
    await waitFor(() =>
      expect(screen.getByTestId("settings-identity-status")).toHaveAttribute("data-phase", "saved"),
    );
    const saved = screen.getByTestId("settings-identity-status");
    expect(saved).toHaveTextContent("已保存");
    expect(JSON.parse(localStorage.getItem("runtime.device-settings.v1")!).deviceName).toBe("desk-2");
  });

  it("push toggle failure reports 失败 and rolls the button back to the real state", async () => {
    pushMock.subscribePush.mockRejectedValueOnce(new Error("denied by browser"));
    renderSettings(["/settings"]);
    fireEvent.click(screen.getByTestId("settings-push"));
    await waitFor(() =>
      expect(screen.getByTestId("settings-notifications-status")).toHaveAttribute("data-phase", "error"),
    );
    // Re-reading status restores the last known (unsubscribed) value.
    expect(screen.getByTestId("settings-push")).toHaveTextContent("开启推送");
    expect(screen.getByTestId("settings-push")).toHaveAttribute("aria-pressed", "false");
  });

  it("reset discards an unsaved identity draft and the pending status", async () => {
    renderSettings(["/settings"]);
    fireEvent.change(screen.getByTestId("settings-device-name"), { target: { value: "desk-9" } });
    expect(screen.getByTestId("settings-identity-save")).toBeEnabled();
    fireEvent.click(screen.getByTestId("settings-identity-reset"));
    expect(screen.getByTestId("settings-device-name")).toHaveValue("this-device");
    expect(screen.getByTestId("settings-identity-save")).toBeDisabled();
  });
});

describe("kept sections", () => {
  it("keeps the passkey section and its add/row controls", () => {
    vi.spyOn(store, "useHub").mockReturnValue({
      ...baseHub,
      passkeys: [
        {
          id: "psk_1",
          name: "Chrome on Linux",
          createdAt: "2026-09-01T00:00:00Z",
          lastUsedAt: null,
          thisDevice: true,
        },
      ],
    });
    renderSettings(["/settings"]);
    expect(screen.getByTestId("settings-passkeys")).toBeInTheDocument();
    expect(screen.getByTestId("settings-passkey-add")).toBeInTheDocument();
    expect(screen.getByTestId("settings-passkey-name")).toBeInTheDocument();
    expect(screen.getByTestId("settings-passkey-row")).toHaveTextContent("本机");
    expect(screen.getByTestId("settings-passkey-rename")).toBeInTheDocument();
    expect(screen.getByTestId("settings-passkey-delete")).toBeInTheDocument();
  });

  it("keeps the management links, providers included", () => {
    renderSettings(["/settings"]);
    const providers = screen.getByRole("link", { name: "Provider" });
    expect(providers).toHaveAttribute("href", "/providers");
    expect(screen.getByRole("link", { name: "主机" })).toHaveAttribute("href", "/hosts");
  });
});
