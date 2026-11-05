import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

/**
 * D-062: every close entry point goes through hubStore.close. For a claimed
 * external herdr pane that must be the release route (detach only), never
 * the instance.close command; an ordinary instance keeps instance.close.
 */
type Internals = { state: { instances: Instance[] } };

function withInstances(rows: Instance[]) {
  const store = hubStore as unknown as Internals;
  const before = store.state;
  store.state = { ...store.state, instances: rows };
  return () => {
    store.state = before;
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

it("closing a claimed herdr pane releases it and never sends instance.close", async () => {
  const base = mockDb.instances[0];
  const claimed: Instance = {
    ...base,
    id: "ins_claimed_release",
    driver: "herdr-claimed",
    herdrClaim: { session: "work", workspaceId: "w2", tabId: "w2:t19", paneId: "w2:p5K" },
  };
  const plain: Instance = { ...base, id: "ins_plain_close", driver: "claude-pty" };
  const restore = withInstances([claimed, plain]);
  const release = vi.spyOn(api, "instanceRelease").mockResolvedValue({ released: true });
  const close = vi.spyOn(api, "instanceClose").mockResolvedValue({} as Awaited<ReturnType<typeof api.instanceClose>>);
  vi.spyOn(hubStore, "refresh").mockResolvedValue(undefined);
  try {
    await hubStore.close(claimed.id);
    expect(release).toHaveBeenCalledWith(claimed.id);
    expect(close).not.toHaveBeenCalled();
    await hubStore.close(plain.id);
    expect(close).toHaveBeenCalledWith(plain.id);
    expect(release).toHaveBeenCalledTimes(1);
  } finally {
    restore();
  }
});
