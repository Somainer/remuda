import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import * as api from "../../lib/api";
import type { Id } from "../../types/wire";
import { HostDiagnostics } from "./HostDiagnostics";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("surfaces Node permission denial and refreshes after access is repaired", async () => {
  const check = vi.spyOn(api, "fetchHostDoctor")
    .mockResolvedValueOnce({ exitCode: 1, checks: [{ name: "workspace.access", status: "blocker", message: "macOS denied access; grant Full Disk Access to remuda in System Settings → Privacy & Security" }] })
    .mockResolvedValueOnce({ exitCode: 0, checks: [{ name: "workspace.access", status: "ok", message: "Node can read the registered workspace" }] });
  render(<HostDiagnostics hostId={"host-fixture" as Id} online />);
  expect(await screen.findByRole("alert")).toHaveTextContent("grant Full Disk Access");
  expect(check).toHaveBeenCalledWith("host-fixture");
  fireEvent.click(screen.getByRole("button", { name: "重新检查" }));
  expect(await screen.findByText("主机检查通过")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("reports failed diagnostics and does not probe an offline Node", async () => {
  const check = vi.spyOn(api, "fetchHostDoctor").mockRejectedValue(new Error("Node diagnostics timed out"));
  const view = render(<HostDiagnostics hostId={"host-fixture" as Id} online={false} />);
  expect(screen.getByRole("status")).toHaveTextContent("主机离线");
  expect(check).not.toHaveBeenCalled();
  view.rerender(<HostDiagnostics hostId={"host-fixture" as Id} online />);
  await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("timed out"));
  expect(screen.queryByText("主机检查通过")).not.toBeInTheDocument();
});

it("shows an explicit error for a successful HTTP response without doctor checks", async () => {
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ ok: true }), { status: 200 }));
  vi.stubGlobal("fetch", fetch);
  render(<HostDiagnostics hostId={"host-fixture" as Id} online />);
  expect(await screen.findByRole("alert")).toHaveTextContent("无效的诊断报告");
  expect(fetch).toHaveBeenCalledOnce();
  expect(screen.queryByText("主机检查通过")).not.toBeInTheDocument();
});
