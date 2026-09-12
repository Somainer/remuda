import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { BroadcastBox } from "./BroadcastBox";
import { api } from "../../lib/api";
import type { Instance } from "../../types/instance";
import type { Id } from "../../types/wire";
import type { HostView } from "../hosts/model";

const hosts = [{ id: "hst_1" as Id, label: "sg-node" }] as HostView[];
const instances = [{ id: "ins_1" as Id, kind: "claude" }, { id: "ins_2" as Id, kind: "codex" }] as Instance[];

describe("BroadcastBox", () => {
  it("posts the typed prompt and renders per-instance results", async () => {
    const user = userEvent.setup();
    const broadcast = vi.spyOn(api, "fleetBroadcast").mockResolvedValue({
      accepted: 1,
      failed: 1,
      skipped: 0,
      results: [
        { instanceId: "ins_1", hostId: "hst_1", kind: "claude", ok: true, state: "accepted" },
        { instanceId: "ins_2", hostId: "hst_1", kind: "codex", ok: false, error: "host offline" },
      ],
    });

    render(<BroadcastBox hosts={hosts} instances={instances} />);
    await user.type(screen.getByTestId("broadcast-text"), "PAUSE git commits");
    await user.click(screen.getByTestId("broadcast-send"));

    await waitFor(() => expect(screen.getByTestId("broadcast-summary")).toBeInTheDocument());
    expect(broadcast).toHaveBeenCalledOnce();
    const body = broadcast.mock.calls[0][0];
    expect(body.operation).toBe("instance.send");
    expect(body.all).toBe(true);

    expect(screen.getByTestId("broadcast-summary").textContent).toContain("已接受 1");
    const rows = screen.getAllByTestId("broadcast-result");
    expect(rows).toHaveLength(2);
    // Failures sort first and surface their reason.
    expect(rows[0]).toHaveAttribute("data-ok", "false");
    expect(rows[0].textContent).toContain("host offline");
    broadcast.mockRestore();
  });

  it("applies the host and kind filter to the request", async () => {
    const user = userEvent.setup();
    const broadcast = vi
      .spyOn(api, "fleetBroadcast")
      .mockResolvedValue({ accepted: 0, failed: 0, skipped: 2, results: [] });

    render(<BroadcastBox hosts={hosts} instances={instances} />);
    await user.selectOptions(screen.getByTestId("broadcast-host"), "hst_1");
    await user.selectOptions(screen.getByTestId("broadcast-kind"), "codex");
    await user.type(screen.getByTestId("broadcast-text"), "resume");
    await user.click(screen.getByTestId("broadcast-send"));

    await waitFor(() => expect(broadcast).toHaveBeenCalledOnce());
    const body = broadcast.mock.calls[0][0];
    expect(body.hosts).toEqual(["hst_1"]);
    expect(body.kinds).toEqual(["codex"]);
    broadcast.mockRestore();
  });

  it("sends a key instead of a prompt in key mode", async () => {
    const user = userEvent.setup();
    const broadcast = vi
      .spyOn(api, "fleetBroadcast")
      .mockResolvedValue({ accepted: 2, failed: 0, skipped: 0, results: [] });

    render(<BroadcastBox hosts={hosts} instances={instances} />);
    await user.selectOptions(screen.getByTestId("broadcast-mode"), "key");
    await user.selectOptions(screen.getByTestId("broadcast-key"), "esc");
    await user.click(screen.getByTestId("broadcast-send"));

    await waitFor(() => expect(broadcast).toHaveBeenCalledOnce());
    const body = broadcast.mock.calls[0][0];
    expect(body.operation).toBe("tty.write");
    expect((body.payload as { keys: string[] }).keys).toEqual(["esc"]);
    broadcast.mockRestore();
  });

  it("refuses an empty prompt without calling the Hub", async () => {
    const user = userEvent.setup();
    const broadcast = vi.spyOn(api, "fleetBroadcast");

    render(<BroadcastBox hosts={hosts} instances={instances} />);
    await user.click(screen.getByTestId("broadcast-send"));

    expect(await screen.findByTestId("broadcast-error")).toBeInTheDocument();
    expect(broadcast).not.toHaveBeenCalled();
    broadcast.mockRestore();
  });

  it("surfaces a failed request as an error", async () => {
    const user = userEvent.setup();
    const broadcast = vi.spyOn(api, "fleetBroadcast").mockRejectedValue(new Error("hub HTTP 400"));

    render(<BroadcastBox hosts={hosts} instances={instances} />);
    await user.type(screen.getByTestId("broadcast-text"), "x");
    await user.click(screen.getByTestId("broadcast-send"));

    expect((await screen.findByTestId("broadcast-error")).textContent).toContain("hub HTTP 400");
    broadcast.mockRestore();
  });
});
