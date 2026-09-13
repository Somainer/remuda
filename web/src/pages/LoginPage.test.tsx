import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { hubStore } from "../lib/store";
import { LoginPage } from "./LoginPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false }),
}));

function renderLogin() {
  return render(
    <MemoryRouter>
      <LoginPage />
    </MemoryRouter>,
  );
}

describe("LoginPage passkey-primary layout", () => {
  beforeEach(() => {
    localStorage.clear();
  });
  afterEach(() => vi.restoreAllMocks());

  it("shows the passkey button first and collapses the access-code form", async () => {
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(true);
    vi.spyOn(hubStore, "conditionalMediationAvailable").mockResolvedValue(false);
    const passkeyLogin = vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue(undefined);

    renderLogin();
    expect(screen.getByTestId("login-passkey-submit")).toBeVisible();
    expect(screen.queryByTestId("login-tab-bootstrap")).not.toBeInTheDocument();

    fireEvent.click(screen.getByTestId("login-passkey-submit"));
    await waitFor(() => expect(passkeyLogin).toHaveBeenCalledWith("required", "this-device"));
  });

  it("reveals the preserved bootstrap/pair form behind 使用访问码 and submits bootstrap", async () => {
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(true);
    vi.spyOn(hubStore, "conditionalMediationAvailable").mockResolvedValue(false);
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue(undefined);
    const login = vi.spyOn(hubStore, "login").mockResolvedValue(undefined);

    renderLogin();
    fireEvent.click(screen.getByTestId("login-use-code"));

    const token = screen.getByTestId("login-bootstrap-token");
    const name = screen.getByTestId("login-device-name");
    fireEvent.change(token, { target: { value: "secret-code" } });
    fireEvent.change(name, { target: { value: "my-device" } });
    fireEvent.click(screen.getByTestId("login-submit"));
    await waitFor(() => expect(login).toHaveBeenCalledWith("bootstrap", "secret-code", "my-device"));
  });

  it("keeps the pair tab working", async () => {
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(true);
    vi.spyOn(hubStore, "conditionalMediationAvailable").mockResolvedValue(false);
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue(undefined);
    const login = vi.spyOn(hubStore, "login").mockResolvedValue(undefined);

    renderLogin();
    fireEvent.click(screen.getByTestId("login-use-code"));
    fireEvent.click(screen.getByTestId("login-tab-pair"));
    fireEvent.change(screen.getByTestId("login-pair-code"), { target: { value: "abcd1234" } });
    fireEvent.click(screen.getByTestId("login-submit"));
    await waitFor(() => expect(login).toHaveBeenCalledWith("pair", "ABCD1234", expect.any(String)));
  });

  it("expands the access-code form and explains the fallback when WebAuthn is missing", () => {
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(false);
    renderLogin();
    expect(screen.getByTestId("login-passkey-unsupported")).toBeVisible();
    expect(screen.getByTestId("login-tab-bootstrap")).toBeVisible();
    expect(screen.queryByTestId("login-use-code")).not.toBeInTheDocument();
  });

  it("surfaces passkey failure copy in login-error", async () => {
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(true);
    vi.spyOn(hubStore, "conditionalMediationAvailable").mockResolvedValue(false);
    vi.spyOn(hubStore, "passkeyLogin").mockRejectedValue(new Error("当前没有可用的 Passkey 或验证设备未响应。"));

    renderLogin();
    fireEvent.click(screen.getByTestId("login-passkey-submit"));
    await waitFor(() => expect(screen.getByTestId("login-error")).toHaveTextContent("Passkey"));
  });
});
