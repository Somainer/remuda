import { afterEach, describe, expect, it } from "vitest";
import { DEVICE_KEY } from "./interactionStatus";
import { clearSession, readLoggedOut, readSession, writeSession } from "./session";

describe("device session", () => {
  afterEach(() => {
    localStorage.clear();
  });

  it("round-trips a session and marks logout", () => {
    writeSession({ deviceId: "dev_1", token: "tok_1", name: "desk" });
    expect(readSession()).toEqual({ deviceId: "dev_1", token: "tok_1", name: "desk" });
    expect(localStorage.getItem(DEVICE_KEY)).toBe("dev_1");
    expect(readLoggedOut()).toBe(false);
    clearSession();
    expect(readSession()).toBeNull();
    expect(readLoggedOut()).toBe(true);
  });
});
