import { afterEach, describe, expect, it } from "vitest";
import { HubHttpError } from "./httpError";
import { mockDeviceList, mockDeviceRevoke, mockLogin, mockPairCode, mockPairRedeem, resetMockAuth } from "./mock";
import { MOCK_BOOTSTRAP_TOKEN } from "./session";

describe("mock device auth", () => {
  afterEach(() => {
    resetMockAuth();
  });

  it("rejects a bad bootstrap token", () => {
    expect(() => mockLogin("nope", "desk")).toThrow(HubHttpError);
  });

  it("issues a pairing code and redeems it once", () => {
    const desk = mockLogin(MOCK_BOOTSTRAP_TOKEN, "desk");
    const issued = mockPairCode(desk.token);
    expect(issued.code).toHaveLength(8);
    const phone = mockPairRedeem(issued.code.toLowerCase(), "phone");
    expect(phone.name).toBe("phone");
    expect(phone.deviceId).not.toBe(desk.deviceId);
    expect(mockDeviceList(desk.token).items).toHaveLength(2);
    expect(() => mockPairRedeem(issued.code, "other")).toThrow(HubHttpError);
    mockDeviceRevoke(desk.token, phone.deviceId);
    expect(mockDeviceList(desk.token).items).toHaveLength(1);
  });
});

import {
  mockPasskeyDelete,
  mockPasskeyList,
  mockPasskeyLoginFinish,
  mockPasskeyLoginStart,
  mockPasskeyRegisterFinish,
  mockPasskeyRegisterStart,
  mockPasskeyRename,
} from "./mock";
import { writeSession } from "./session";

describe("mock passkey auth", () => {
  afterEach(() => {
    resetMockAuth();
    localStorage.removeItem("runtime.device-session");
  });

  it("registers, lists, renames, logs in and deletes", () => {
    const desk = mockLogin(MOCK_BOOTSTRAP_TOKEN, "desk");
    writeSession(desk, { mock: true });

    const start = mockPasskeyRegisterStart("Chrome on macOS");
    expect(start.options.publicKey.authenticatorSelection.residentKey).toBe("required");
    const saved = mockPasskeyRegisterFinish(desk.deviceId);
    expect(saved.name).toBe("Chrome on macOS");
    expect(saved.thisDevice).toBe(true);

    expect(mockPasskeyList(desk.token).items).toHaveLength(1);
    const renamed = mockPasskeyRename(desk.token, saved.id, "work key");
    expect(renamed.name).toBe("work key");

    const loginStart = mockPasskeyLoginStart("conditional");
    expect(loginStart.options.mediation).toBe("conditional");
    expect(mockPasskeyLoginStart().options.mediation).toBeUndefined();

    const passkeySession = mockPasskeyLoginFinish(undefined, saved.id);
    expect(passkeySession.name).toBe("work key");
    expect(mockPasskeyList(desk.token).items[0]?.lastUsedAt).not.toBeNull();

    expect(mockPasskeyDelete(desk.token, saved.id)).toEqual({ ok: true });
    expect(mockPasskeyList(desk.token).items).toHaveLength(0);
    expect(() => mockPasskeyRename(desk.token, saved.id, "x")).toThrow(HubHttpError);
  });

  it("rejects passkey management without a device session", () => {
    localStorage.removeItem("runtime.device-session");
    expect(() => mockPasskeyRegisterStart("k")).toThrow(HubHttpError);
    expect(() => mockPasskeyList(undefined)).toThrow(HubHttpError);
  });
});
