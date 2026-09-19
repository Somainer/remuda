import { describe, expect, it } from "vitest";
import { resolveLanding } from "./mobileRoute";

describe("resolveLanding — compact landing resolution", () => {
  it("bounces the desktop index routes into the /m tree", () => {
    expect(resolveLanding("/sessions", "", true)).toBe("/m");
    expect(resolveLanding("/approvals", "", true)).toBe("/m/inbox");
  });

  it("treats a single trailing slash as absent", () => {
    expect(resolveLanding("/sessions/", "", true)).toBe("/m");
    expect(resolveLanding("/approvals/", "?focus=abc", true)).toBe("/m/inbox?focus=abc");
    expect(resolveLanding("/m/inbox/", "", false)).toBe("/sessions");
    expect(resolveLanding("/m/", "", false)).toBe("/sessions");
    // Shared routes keep not redirecting with or without the slash.
    expect(resolveLanding("/s/ins_1/", "", true)).toBeNull();
    expect(resolveLanding("/settings/", "", false)).toBeNull();
    // The bare root is untouched.
    expect(resolveLanding("/", "", true)).toBe("/m");
  });

  it("keeps the query string verbatim, without parsing or rebuilding it", () => {
    expect(resolveLanding("/approvals", "?focus=abc", true)).toBe("/m/inbox?focus=abc");
    expect(resolveLanding("/approvals", "?focus=abc&kind=approval", true)).toBe(
      "/m/inbox?focus=abc&kind=approval",
    );
    // Percent-encoding and ordering must survive untouched (E8).
    expect(resolveLanding("/approvals", "?kind=question&focus=int_%E2%80%A6", true)).toBe(
      "/m/inbox?kind=question&focus=int_%E2%80%A6",
    );
    expect(resolveLanding("/sessions", "?host=hst_1&workspace=ws_2", true)).toBe(
      "/m?host=hst_1&workspace=ws_2",
    );
  });

  it("never rewrites /s/:id and its sub-views", () => {
    expect(resolveLanding("/s/ins_1", "", true)).toBeNull();
    expect(resolveLanding("/s/ins_1", "?focus=int_1", true)).toBeNull();
    expect(resolveLanding("/s/ins_1/tty", "", true)).toBeNull();
    expect(resolveLanding("/s/ins_1/structured", "", true)).toBeNull();
    expect(resolveLanding("/s/ins_1/files", "", true)).toBeNull();
    expect(resolveLanding("/s/ins_1/events", "", true)).toBeNull();
  });

  it("never rewrites the shared routes on either side", () => {
    for (const path of ["/sessions/new", "/login", "/pair", "/settings"]) {
      expect(resolveLanding(path, "", true), `${path} compact`).toBeNull();
      expect(resolveLanding(path, "", false), `${path} desktop`).toBeNull();
    }
    expect(resolveLanding("/sessions/new", "?host=hst_1", true)).toBeNull();
  });

  it("keeps the phone tree and other workbench routes in place while compact", () => {
    expect(resolveLanding("/m", "", true)).toBeNull();
    expect(resolveLanding("/m", "?x=1", true)).toBeNull();
    expect(resolveLanding("/m/inbox", "?focus=abc", true)).toBeNull();
    expect(resolveLanding("/hosts", "", true)).toBeNull();
    expect(resolveLanding("/projects/ws_1", "", true)).toBeNull();
  });

  it("picks /m for the bare start_url", () => {
    expect(resolveLanding("/", "", true)).toBe("/m");
  });
});

describe("resolveLanding — desktop reverse redirect", () => {
  it("bounces every /m* path back to /sessions", () => {
    expect(resolveLanding("/m", "", false)).toBe("/sessions");
    expect(resolveLanding("/m/inbox", "", false)).toBe("/sessions");
    expect(resolveLanding("/m/inbox", "?focus=abc", false)).toBe("/sessions");
    expect(resolveLanding("/m/anything", "", false)).toBe("/sessions");
  });

  it("leaves the desktop index and session routes untouched", () => {
    expect(resolveLanding("/sessions", "", false)).toBeNull();
    expect(resolveLanding("/sessions", "?status=idle", false)).toBeNull();
    expect(resolveLanding("/approvals", "?focus=abc", false)).toBeNull();
    expect(resolveLanding("/s/ins_1", "", false)).toBeNull();
    expect(resolveLanding("/s/ins_1/tty", "", false)).toBeNull();
  });

  it("picks /sessions for the bare start_url", () => {
    expect(resolveLanding("/", "", false)).toBe("/sessions");
  });
});
