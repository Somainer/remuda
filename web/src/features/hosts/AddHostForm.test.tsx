import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { AddHostForm } from "./AddHostForm";
import { api } from "../../lib/api";
import { hubStore } from "../../lib/store";

afterEach(() => vi.restoreAllMocks());
it("registers through the Hub and refreshes real host status", async () => {
  const add = vi.spyOn(api, "hostSshAdd").mockResolvedValue({} as never);
  const refresh = vi.spyOn(hubStore, "refreshHosts").mockResolvedValue();
  const close = vi.fn();
  render(<AddHostForm open onClose={close} />);
  fireEvent.change(screen.getByTestId("add-host-target"), { target: { value: "dev@sg.example" } });
  fireEvent.change(screen.getByTestId("add-host-label"), { target: { value: "SG" } });
  fireEvent.change(screen.getByTestId("add-host-labels"), { target: { value: "egress:gateway, region:sg" } });
  fireEvent.change(screen.getByTestId("add-host-policy"), { target: { value: "upload_if_missing" } });
  fireEvent.click(screen.getByTestId("add-host-submit"));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(add).toHaveBeenCalledWith({ target: "dev@sg.example", label: "SG", labels: ["egress:gateway", "region:sg"], remuda_binary_policy: "upload_if_missing" });
  expect(refresh).toHaveBeenCalledOnce();
});
it("keeps the dialog open with API errors instead of fabricating online status", async () => {
  vi.spyOn(api, "hostSshAdd").mockRejectedValue(new Error("SSH target already registered"));
  const close = vi.fn();
  render(<AddHostForm open onClose={close} />);
  fireEvent.change(screen.getByTestId("add-host-target"), { target: { value: "sg.example" } });
  fireEvent.click(screen.getByTestId("add-host-submit"));
  expect(await screen.findByRole("alert")).toHaveTextContent("already registered");
  expect(close).not.toHaveBeenCalled();
});
