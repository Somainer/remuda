import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import { mockDb } from "../../lib/mock";
import { hubStore } from "../../lib/store";
import { HostTuiDefaults } from "./HostTuiDefaults";

afterEach(() => vi.restoreAllMocks());

it("saves the chosen host renderer and refreshes confirmed defaults", async () => {
  const host = { ...mockDb.hosts[0], defaultTui: "default" as const };
  const patch = vi.spyOn(api, "hostPatch").mockResolvedValue({ ...host, defaultTui: "fullscreen" });
  const refresh = vi.spyOn(hubStore, "refreshHosts").mockResolvedValue(undefined);
  render(<HostTuiDefaults hosts={[host]} />);
  expect(screen.getByLabelText(host.label)).toHaveValue("default");
  fireEvent.change(screen.getByLabelText(host.label), { target: { value: "fullscreen" } });
  await waitFor(() => expect(patch).toHaveBeenCalledWith(host.id, { defaultTui: "fullscreen" }));
  await waitFor(() => expect(refresh).toHaveBeenCalled());
});

it("shows the save error and keeps the last confirmed host default", async () => {
  const host = { ...mockDb.hosts[0], defaultTui: "default" as const };
  vi.spyOn(api, "hostPatch").mockRejectedValue(new Error("主机不可用"));
  render(<HostTuiDefaults hosts={[host]} />);
  fireEvent.change(screen.getByLabelText(host.label), { target: { value: "fullscreen" } });
  expect(await screen.findByRole("alert")).toHaveTextContent("主机不可用");
  expect(screen.getByLabelText(host.label)).toHaveValue("default");
});
