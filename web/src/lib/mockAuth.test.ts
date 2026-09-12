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
