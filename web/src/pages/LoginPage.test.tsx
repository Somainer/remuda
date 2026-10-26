import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
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
    const passkeyLogin = vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("settled");

    renderLogin();
    expect(screen.getByTestId("login-passkey-submit")).toBeVisible();
    expect(screen.queryByTestId("login-tab-bootstrap")).not.toBeInTheDocument();

    fireEvent.click(screen.getByTestId("login-passkey-submit"));
    await waitFor(() =>
      expect(passkeyLogin).toHaveBeenCalledWith("required", "this-device", expect.any(AbortSignal)),
    );
  });

  it("reveals the preserved bootstrap/pair form behind 使用访问码 and submits bootstrap", async () => {
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(true);
    vi.spyOn(hubStore, "conditionalMediationAvailable").mockResolvedValue(false);
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("settled");
    const login = vi.spyOn(hubStore, "login").mockResolvedValue("settled");

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
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("settled");
    const login = vi.spyOn(hubStore, "login").mockResolvedValue("settled");

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

function RoutedLogin() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route path="/pair" element={<LoginPage mode="pair" />} />
      <Route path="/sessions" element={<div data-testid="sessions-landing">sessions</div>} />
    </Routes>
  );
}

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <RoutedLogin />
    </MemoryRouter>,
  );
}

describe("GATE50 item 3: retired explicit authentication does not navigate", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.spyOn(hubStore, "passkeysSupported").mockReturnValue(true);
    vi.spyOn(hubStore, "conditionalMediationAvailable").mockResolvedValue(false);
  });
  afterEach(() => vi.restoreAllMocks());

  it("retired bootstrap login keeps the login form and does not enter the gated tree", async () => {
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("settled");
    vi.spyOn(hubStore, "stateAuthed").mockReturnValue(false);
    const login = vi.spyOn(hubStore, "login").mockResolvedValue("retired");

    renderAt("/login");
    fireEvent.click(screen.getByTestId("login-use-code"));
    fireEvent.change(screen.getByTestId("login-bootstrap-token"), { target: { value: "secret" } });
    fireEvent.click(screen.getByTestId("login-submit"));
    await waitFor(() => expect(login).toHaveBeenCalledWith("bootstrap", "secret", expect.any(String)));

    // Busy clears; the form stays mounted; no gated destination rendered.
    const submit = screen.getByTestId("login-submit");
    await waitFor(() => expect(submit).not.toBeDisabled());
    expect(screen.getByTestId("login-bootstrap-token")).toBeVisible();
    expect(screen.queryByTestId("sessions-landing")).not.toBeInTheDocument();
  });

  it("retired pairing keeps /pair and the pair form instead of navigating away", async () => {
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("settled");
    vi.spyOn(hubStore, "stateAuthed").mockReturnValue(false);
    const login = vi.spyOn(hubStore, "login").mockResolvedValue("retired");

    renderAt("/pair");
    const pairInput = screen.getByTestId("login-pair-code");
    fireEvent.change(pairInput, { target: { value: "abcd1234" } });
    fireEvent.click(screen.getByTestId("login-submit"));
    await waitFor(() => expect(login).toHaveBeenCalledWith("pair", "ABCD1234", expect.any(String)));

    const submit = screen.getByTestId("login-submit");
    await waitFor(() => expect(submit).not.toBeDisabled());
    expect(screen.getByTestId("login-pair-code")).toBeVisible();
    expect(screen.queryByTestId("sessions-landing")).not.toBeInTheDocument();
  });

  it("a retired passkey submission does not navigate and re-enables the passkey button", async () => {
    const passkeyLogin = vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("retired");
    vi.spyOn(hubStore, "stateAuthed").mockReturnValue(false);

    renderAt("/login");
    const button = screen.getByTestId("login-passkey-submit");
    fireEvent.click(button);
    await waitFor(() => expect(passkeyLogin).toHaveBeenCalledWith("required", expect.any(String), expect.any(AbortSignal)));
    await waitFor(() => expect(button).not.toBeDisabled());
    expect(screen.queryByTestId("sessions-landing")).not.toBeInTheDocument();
  });

  it("a settled, authenticated submission still navigates to the destination", async () => {
    vi.spyOn(hubStore, "passkeyLogin").mockResolvedValue("settled");
    vi.spyOn(hubStore, "stateAuthed").mockReturnValue(true);
    const login = vi.spyOn(hubStore, "login").mockResolvedValue("settled");

    renderAt("/login");
    fireEvent.click(screen.getByTestId("login-use-code"));
    fireEvent.change(screen.getByTestId("login-bootstrap-token"), { target: { value: "secret" } });
    fireEvent.click(screen.getByTestId("login-submit"));
    await waitFor(() => expect(login).toHaveBeenCalled());
    expect(await screen.findByTestId("sessions-landing")).toBeVisible();
  });
});
