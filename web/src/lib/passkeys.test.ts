import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  base64UrlDecode,
  base64UrlEncode,
  conditionalMediationAvailable,
  createPasskey,
  defaultPasskeyName,
  getPasskey,
  passkeysSupported,
  PasskeyError,
  passkeyErrorText,
  type ServerCreationOptions,
  type ServerRequestOptions,
} from "./passkeys";

const b64 = (bytes: number[]) => base64UrlEncode(new Uint8Array(bytes));

// vi.fn() infers no-arg calls here; the credential mocks do take one options arg.
const firstCall = (fn: ReturnType<typeof vi.fn>) => (fn.mock.calls[0] as unknown[])[0] as Record<string, unknown>;

describe("base64url", () => {
  it("round-trips binary without padding", () => {
    const data = new Uint8Array([0, 1, 2, 250, 251, 252, 253, 254, 255]);
    const text = base64UrlEncode(data);
    expect(text).not.toMatch(/[+/=]/);
    expect(Array.from(base64UrlDecode(text))).toEqual(Array.from(data));
  });

  it("decodes unpadded url-safe alphabet", () => {
    // {251,255} -> base64url "_/8" style chars
    expect(Array.from(base64UrlDecode(b64([251, 255])))).toEqual([251, 255]);
    expect(Array.from(base64UrlDecode("YWJj"))).toEqual([97, 98, 99]);
  });
});

describe("defaultPasskeyName", () => {
  it("recognises desktop Chrome on macOS", () => {
    const ua =
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36";
    expect(defaultPasskeyName(ua)).toBe("Chrome on macOS");
  });

  it("recognises mobile Safari on iPhone and Edge on Windows", () => {
    const iphone =
      "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
    expect(defaultPasskeyName(iphone)).toBe("Safari on iPhone");
    const edge =
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36 Edg/139.0.0.0";
    expect(defaultPasskeyName(edge)).toBe("Edge on Windows");
  });
});

describe("feature detection", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("is false without PublicKeyCredential", () => {
    vi.stubGlobal("PublicKeyCredential", undefined);
    expect(passkeysSupported()).toBe(false);
  });

  it("is true on a secure context with the constructor", () => {
    vi.stubGlobal("PublicKeyCredential", function PublicKeyCredential() {});
    expect(passkeysSupported()).toBe(true);
  });

  it("reports conditional mediation from the static probe", async () => {
    const probe = vi.fn(async () => true);
    vi.stubGlobal("PublicKeyCredential", function PublicKeyCredential() {});
    (window.PublicKeyCredential as unknown as { isConditionalMediationAvailable: () => Promise<boolean> })
      .isConditionalMediationAvailable = probe;
    await expect(conditionalMediationAvailable()).resolves.toBe(true);
    expect(probe).toHaveBeenCalledOnce();
  });

  it("is false when the probe throws", async () => {
    vi.stubGlobal("PublicKeyCredential", function PublicKeyCredential() {});
    Object.defineProperty(window.PublicKeyCredential, "isConditionalMediationAvailable", {
      configurable: true,
      value: () => Promise.reject(new Error("blocked")),
    });
    await expect(conditionalMediationAvailable()).resolves.toBe(false);
  });
});

function fakeAttestation() {
  return {
    attestationObject: new Uint8Array([1, 2, 3]),
    clientDataJSON: new Uint8Array([4, 5, 6]),
    getTransports: () => ["internal"],
  };
}

function fakeAssertion() {
  return {
    authenticatorData: new Uint8Array([7, 8]),
    clientDataJSON: new Uint8Array([9]),
    signature: new Uint8Array([10, 11]),
    userHandle: new Uint8Array([12, 13]),
  };
}

describe("createPasskey", () => {
  afterEach(() => vi.unstubAllGlobals());

  const serverOptions: ServerCreationOptions = {
    publicKey: {
      challenge: b64([1, 2, 3, 4]),
      user: { id: b64([9, 9, 9]) },
      excludeCredentials: [{ id: b64([5, 6]) }],
      rp: { name: "Remuda", id: "localhost" },
    },
  };

  it("decodes server fields and encodes the attestation response", async () => {
    const create = vi.fn(async () => ({
      id: "cred-1",
      type: "public-key",
      rawId: new Uint8Array([20, 21]),
      response: fakeAttestation(),
    }));
    vi.stubGlobal("PublicKeyCredential", function PublicKeyCredential() {});
    Object.defineProperty(globalThis.navigator, "credentials", { configurable: true, value: { create } });

    const body = await createPasskey(serverOptions);
    expect(body).toMatchObject({ id: "cred-1", type: "public-key" });
    expect(body.rawId).toBe(base64UrlEncode(new Uint8Array([20, 21])));
    expect(Array.from(base64UrlDecode(body.response.attestationObject))).toEqual([1, 2, 3]);
    expect(Array.from(base64UrlDecode(body.response.clientDataJSON))).toEqual([4, 5, 6]);
    expect(body.response.transports).toEqual(["internal"]);

    const passed = firstCall(create).publicKey as {
      challenge: unknown;
      user: { id: unknown };
      excludeCredentials: { id: unknown }[];
    };
    expect(passed.challenge).toBeInstanceOf(Uint8Array);
    expect(passed.user.id).toBeInstanceOf(Uint8Array);
    expect(passed.excludeCredentials[0]?.id).toBeInstanceOf(Uint8Array);
  });

  it("normalizes a dismissed ceremony to cancelled", async () => {
    const create = vi.fn(async () => {
      throw new DOMException("dismissed", "NotAllowedError");
    });
    vi.stubGlobal("PublicKeyCredential", function PublicKeyCredential() {});
    Object.defineProperty(globalThis.navigator, "credentials", { configurable: true, value: { create } });

    await expect(createPasskey(serverOptions)).rejects.toMatchObject({ kind: "cancelled" });
  });

  it("throws unsupported when WebAuthn is missing", async () => {
    vi.stubGlobal("PublicKeyCredential", undefined);
    await expect(createPasskey(serverOptions)).rejects.toMatchObject({ kind: "unsupported" });
  });
});

describe("getPasskey", () => {
  beforeEach(() => {
    vi.stubGlobal("PublicKeyCredential", function PublicKeyCredential() {});
  });
  afterEach(() => vi.unstubAllGlobals());

  const serverOptions: ServerRequestOptions = {
    publicKey: { challenge: b64([1, 2, 3]), allowCredentials: [] },
  };

  it("forwards required mediation and the abort signal and encodes the assertion", async () => {
    const get = vi.fn(async () => ({
      id: "cred-2",
      type: "public-key",
      rawId: new Uint8Array([30, 31]),
      response: fakeAssertion(),
    }));
    Object.defineProperty(globalThis.navigator, "credentials", { configurable: true, value: { get } });
    const signal = new AbortController().signal;

    const body = await getPasskey(serverOptions, "required", signal);
    expect(Array.from(base64UrlDecode(body.response.authenticatorData))).toEqual([7, 8]);
    expect(Array.from(base64UrlDecode(body.response.signature))).toEqual([10, 11]);
    expect(Array.from(base64UrlDecode(body.response.userHandle ?? ""))).toEqual([12, 13]);
    expect(firstCall(get).mediation).toBe("required");
    expect(firstCall(get).signal).toBe(signal);
  });

  it("uses conditional mediation when asked and sends null userHandle", async () => {
    const get = vi.fn(async () => ({
      id: "cred-3",
      type: "public-key",
      rawId: new Uint8Array([1]),
      response: { ...fakeAssertion(), userHandle: null },
    }));
    Object.defineProperty(globalThis.navigator, "credentials", { configurable: true, value: { get } });

    const body = await getPasskey(serverOptions, "conditional");
    expect(body.response.userHandle).toBeNull();
    expect(firstCall(get).mediation).toBe("conditional");
  });

  it("maps InvalidStateError to unavailable", async () => {
    const get = vi.fn(async () => {
      throw new DOMException("no credential", "InvalidStateError");
    });
    Object.defineProperty(globalThis.navigator, "credentials", { configurable: true, value: { get } });
    await expect(getPasskey(serverOptions)).rejects.toBeInstanceOf(PasskeyError);
    await expect(getPasskey(serverOptions)).rejects.toMatchObject({ kind: "unavailable" });
  });
});

describe("passkeyErrorText", () => {
  it("has Chinese copy for every kind", () => {
    expect(passkeyErrorText(new PasskeyError("unsupported", "x"))).toContain("不支持");
    expect(passkeyErrorText(new PasskeyError("cancelled", "x"))).toContain("取消");
    expect(passkeyErrorText(new PasskeyError("unavailable", "x"))).toContain("可用");
    expect(passkeyErrorText(new Error("boom"))).toBe("boom");
  });
});
