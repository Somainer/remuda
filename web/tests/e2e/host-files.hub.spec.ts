import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Read-only host files (host-files slice): the browser/operator side of the
 * Hub -> Node -> objects loop against the in-process fake Node. Everything
 * runs over real HTTP: the fake Node lists seeded /tmp directories and
 * uploads read bytes back through POST .../files/objects with its host
 * token. No live model is involved.
 */
test.describe.configure({ mode: "serial" });

const E2E_TEXT = "remuda host files e2e\n";

async function jsonFetch(page: Page, path: string, init?: RequestInit) {
  return page.evaluate(
    async ({ path, init }) => {
      const response = await fetch(path, { credentials: "include", ...init });
      const text = await response.text();
      let body: unknown = null;
      try {
        body = text ? JSON.parse(text) : null;
      } catch {
        body = text;
      }
      return { status: response.status, body };
    },
    { path, init: init ?? null },
  );
}

async function findFakeNode(page: Page): Promise<string> {
  const { status, body } = await jsonFetch(page, "/v1/hosts");
  expect(status).toBe(200);
  const hosts = body as { items?: { id?: string; label?: string }[] };
  const host = (hosts.items ?? []).find((item) => item.label === "e2e-fake-node");
  if (!host?.id) throw new Error("e2e-fake-node not found");
  return host.id;
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest))
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function base64ToBytes(base64: string): Uint8Array {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test("lists a registered workspace directory", async ({ page }) => {
  const host = await findFakeNode(page);
  const { status, body } = await jsonFetch(
    page,
    `/v1/hosts/${host}/files?workspaceId=wsp_e2e`,
  );
  expect(status).toBe(200);
  const listing = body as {
    path: string;
    entries: { name: string; kind: string; size: number; mtime: number; mode: number }[];
  };
  expect(listing.path).toMatch(/\/tmp\/remuda-e2e$/);
  const byName = Object.fromEntries(listing.entries.map((entry) => [entry.name, entry]));
  expect(byName["host-files-e2e.txt"].kind).toBe("file");
  expect(byName["host-files-e2e.txt"].size).toBe(E2E_TEXT.length);
  expect(byName["host-files-dir"].kind).toBe("dir");
  for (const entry of listing.entries) {
    // Bare names only; no paths leak in the listing.
    expect(entry.name).not.toContain("/");
    expect(typeof entry.mtime).toBe("number");
    expect(typeof entry.mode).toBe("number");
  }

  const sub = await jsonFetch(
    page,
    `/v1/hosts/${host}/files?workspaceId=wsp_e2e&relPath=host-files-dir`,
  );
  expect(sub.status).toBe(200);
  const subListing = sub.body as { entries: { name: string }[] };
  expect(subListing.entries.map((entry) => entry.name)).toContain("inside.txt");
});

test("reads a file end to end through the objects channel", async ({ page }) => {
  const host = await findFakeNode(page);
  const read = await jsonFetch(page, `/v1/hosts/${host}/files/read`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ workspaceId: "wsp_e2e", relPath: "host-files-e2e.txt" }),
  });
  expect(read.status).toBe(200);
  const staged = read.body as { objectId: string; digest: string; size: number };
  expect(staged.objectId).toMatch(/^obj_/);
  expect(staged.size).toBe(E2E_TEXT.length);
  expect(staged.digest).toMatch(/^[0-9a-f]{64}$/);

  // Bytes keep flowing through the existing objects download route. Return
  // base64: an ArrayBuffer does not survive page.evaluate serialization.
  const downloaded = await page.evaluate(async (objectId) => {
    const response = await fetch(`/v1/objects/${objectId}`, { credentials: "include" });
    const buffer = await response.arrayBuffer();
    let binary = "";
    const view = new Uint8Array(buffer);
    for (let index = 0; index < view.length; index += 0x8000) {
      binary += String.fromCharCode(...view.subarray(index, index + 0x8000));
    }
    return { status: response.status, base64: btoa(binary) };
  }, staged.objectId);
  expect(downloaded.status).toBe(200);
  const bytes = base64ToBytes(downloaded.base64);
  expect(new TextDecoder().decode(bytes)).toBe(E2E_TEXT);
  expect(await sha256Hex(bytes)).toBe(staged.digest);
});

test("rejects traversal and out-of-scratch paths", async ({ page }) => {
  const host = await findFakeNode(page);

  const traversal = await jsonFetch(
    page,
    `/v1/hosts/${host}/files?workspaceId=wsp_e2e&relPath=../remuda-e2e-second/other.txt`,
  );
  expect(traversal.status).toBe(400);
  expect(JSON.stringify(traversal.body)).toContain("escapes");

  // An unregistered workspace id is refused by the Node.
  const unknown = await jsonFetch(page, `/v1/hosts/${host}/files?workspaceId=wsp_missing`);
  expect(unknown.status).toBe(400);

  // The scratch area admits only /tmp/remuda-* first components.
  const denied = await jsonFetch(page, `/v1/hosts/${host}/files?workspaceId=tmp&relPath=..`);
  expect(denied.status).toBe(400);
});

test("lists the /tmp/remuda-* scratch area", async ({ page }) => {
  const host = await findFakeNode(page);
  const { status, body } = await jsonFetch(
    page,
    `/v1/hosts/${host}/files?workspaceId=tmp&relPath=remuda-hostfiles-e2e`,
  );
  expect(status).toBe(200);
  const listing = body as { entries: { name: string }[] };
  expect(listing.entries.map((entry) => entry.name)).toContain("scratch.txt");
});

test("unknown host is 404 and an anonymous caller is 401", async ({ page, request }) => {
  const host = await findFakeNode(page);
  const missing = await jsonFetch(
    page,
    `/v1/hosts/hst_00000000-0000-7000-8000-000000000000/files?workspaceId=wsp_e2e`,
  );
  expect(missing.status).toBe(404);

  // Separate context with no device cookie.
  const response = await request.get(
    `/v1/hosts/${host}/files?workspaceId=wsp_e2e`,
  );
  expect(response.status()).toBe(401);
});
