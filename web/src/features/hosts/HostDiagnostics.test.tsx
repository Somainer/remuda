import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import * as api from "../../lib/api";
import type { Id } from "../../types/wire";
import { HostDiagnostics } from "./HostDiagnostics";
import { COMPUTER_USE_KIND } from "./model";

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

// D-045 §3.4 / ui-spec §2.6: the capability is an ordinary CLI row, so the
// host detail's CLI table owns the two *reported* states. The one state that
// table cannot express is the row's absence (both list helpers yield nothing),
// so that is the only case this component draws — rendering the reported
// states here too would show one fact twice on one page.

it("does not repeat a reported row the CLI table already draws", async () => {
  vi.spyOn(api, "fetchHostDoctor").mockResolvedValue({ exitCode: 0, checks: [] });
  const { rerender } = render(<HostDiagnostics hostId={"host-fixture" as Id} online cli={[
    { kind: COMPUTER_USE_KIND, version: "2.7.0", path: "/home/x/client", auth: "unknown", installed: true },
  ]} />);
  expect(screen.queryByTestId("computer-use-row")).not.toBeInTheDocument();

  // Same for the reported-absent state: the CLI table shows 未安装 for it.
  rerender(<HostDiagnostics hostId={"host-fixture" as Id} online cli={[
    { kind: COMPUTER_USE_KIND, auth: "unknown", installed: false },
  ]} />);
  expect(screen.queryByTestId("computer-use-row")).not.toBeInTheDocument();
});

it("renders the row only when the host did not report one at all", async () => {
  vi.spyOn(api, "fetchHostDoctor").mockResolvedValue({ exitCode: 0, checks: [] });
  render(<HostDiagnostics hostId={"host-fixture" as Id} online cli={[
    { kind: "claude", version: "2.1.268", path: "/usr/bin/claude", auth: "logged_in" },
  ]} />);
  const row = screen.getByTestId("computer-use-row");
  expect(row).toHaveAttribute("data-state", "unreported");
  expect(row).toHaveTextContent("未上报");
  // The point: none of the definite claims is made. "未安装" would be this
  // host saying no, and a version would be it saying yes.
  expect(row).not.toHaveTextContent("未安装");
  expect(screen.getByTestId("computer-use-detail")).toHaveTextContent("不代表本机不支持");
});

it("renders the row the same way when no cli inventory arrived at all", () => {
  vi.spyOn(api, "fetchHostDoctor").mockResolvedValue({ exitCode: 0, checks: [] });
  render(<HostDiagnostics hostId={"host-fixture" as Id} online={false} />);
  expect(screen.getByTestId("computer-use-row")).toHaveAttribute("data-state", "unreported");
});
