import { mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * D-047 operator surface (api route): the Session strip must show the route
 * the session actually got — the Node-echoed `apiRoute`, never the requested
 * value — and a mid-flight proxy-host loss renders as an `api-route-down`
 * ERROR while the clause still names the via route: not a reroute (D-035).
 *
 * Harness knob (documented here because it lives in
 * crates/remuda-hub/examples/hub_e2e.rs): this suite runs with
 * HUB_E2E_API_ROUTE=1, which makes the fake worker advertise the api.* relay
 * class and enrolls a second relay-capable fake Node labelled
 * `e2e-via-host`. The port-scoped temp file `remuda-e2e-route-down-<port>`
 * is the smallest possible offline-via-host knob: while it exists the proxy
 * Node holds its WebSocket closed, so the Hub's link-lost hook blocks every
 * via instance with `api-route-down`; deleting it reconnects the proxy.
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/api-route");

test.describe.configure({ mode: "serial" });
test.skip(process.env.HUB_E2E_EXTERNAL === "1", "needs the in-process fake Nodes");
// The proxy fixture and the worker's apiRelay capability exist only when the
// harness was started with HUB_E2E_API_ROUTE=1; a default full-suite run
// (without it) skips this spec so its worker hello stays byte-identical.
test.skip(
  process.env.HUB_E2E_API_ROUTE !== "1",
  "set HUB_E2E_API_ROUTE=1 for the api-route harness",
);

const hubListen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const gatePort = hubListen.split(":")[1] ?? "58880";
const downGate = path.join(os.tmpdir(), `remuda-e2e-route-down-${gatePort}`);

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

test.beforeEach(async () => {
  await rm(downGate, { force: true });
});
test.afterAll(async () => {
  await rm(downGate, { force: true });
});

test("a via session shows its echoed proxy route, then api-route-down when the proxy drops", async ({
  page,
}) => {
  test.setTimeout(120_000);
  await login(page, "e2e-api-route");
  const headers = { Origin: new URL(page.url()).origin };

  const hosts = (await (await page.request.get("/v1/hosts")).json()) as {
    items: { hostId: string; label: string; online?: boolean }[];
  };
  const worker = hosts.items.find((item) => item.label === "e2e-fake-node");
  const proxy = hosts.items.find((item) => item.label === "e2e-via-host");
  expect(worker, "worker fake Node (HUB_E2E_API_ROUTE=1)").toBeTruthy();
  expect(proxy, "proxy fake Node (HUB_E2E_API_ROUTE=1)").toBeTruthy();
  const workerId = worker!.hostId;
  const proxyId = proxy!.hostId;
  await expect
    .poll(
      async () =>
        (await (await page.request.get("/v1/hosts")).json()).items as {
          hostId: string;
          online?: boolean;
        }[],
      { timeout: 20_000 },
    )
    .toEqual(expect.arrayContaining([expect.objectContaining({ hostId: proxyId, online: true })]));

  const workspaces = (await (
    await page.request.get(`/v1/hosts/${workerId}/workspaces`)
  ).json()) as { workspaces: { workspaceId: string; root: string }[] };
  // HUB_E2E_API_ROUTE=1 announces one branded-id workspace for project
  // membership; the legacy wsp_e2e labels are deliberately non-branded.
  const branded = /^wsp_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  const workspace =
    workspaces.workspaces.find((entry) => branded.test(entry.workspaceId)) ??
    workspaces.workspaces[0];
  expect(workspace).toBeTruthy();

  // A project hosting the worker, mirroring the CLI refusal tests.
  const project = await page.request.post("/v1/projects", {
    headers,
    data: { name: "e2e-api-route" },
  });
  expect(project.ok(), await project.text()).toBe(true);
  const projectId = ((await project.json()) as { id: string }).id;
  const workspaceId = workspace.workspaceId;
  const member = await page.request.post(`/v1/projects/${projectId}/members`, {
    headers,
    data: { hostId: workerId, workspaceId, role: "build" },
  });
  expect(member.ok(), await member.text()).toBe(true);
  const quota = await page.request.patch(`/v1/projects/${projectId}`, {
    headers,
    data: {
      hosts: [
        {
          hostId: workerId,
          maxInstances: 8,
          maxBuilding: 4,
          diskBudgetGb: 10,
          portBlocks: ["59200-59229"],
          requires: [],
          latencyClass: "remote",
        },
      ],
    },
  });
  expect(quota.ok(), await quota.text()).toBe(true);

  // Universal gateway profile; the credential is a synthetic delimited fake.
  const profile = await page.request.post("/v1/providers", {
    headers,
    data: {
      name: "e2e-api-route-gateway",
      kind: "gateway",
      baseUrl: `http://${process.env.HUB_E2E_UPSTREAM_LISTEN ?? "127.0.0.1:58881"}/v1`,
      authToken: "sk-fake-profile-0001",
      models: [{ id: "e2e/auto", family: "synth", role: "workhorse", priority: 20 }],
      defaultModel: "e2e/auto",
    },
  });
  expect(profile.ok(), await profile.text()).toBe(true);
  const profileId = ((await profile.json()) as { id: string }).id;

  let instanceId = "";
  try {
    // Dispatch a via session: every model API request should egress on the
    // second fake Node over hub-relay (the proxy has no relayBind).
    const dispatch = await page.request.post("/v1/workers/dispatch", {
      headers,
      data: {
        projectId,
        name: "c-api-route",
        brief:
          "Do the trivial work in your worktree.\n" +
          "Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
        model: "e2e/auto",
        apiVia: proxyId,
      },
    });
    expect(dispatch.ok(), await dispatch.text()).toBe(true);
    const dispatched = (await dispatch.json()) as { instanceId: string };
    instanceId = dispatched.instanceId;
    expect(instanceId).toBeTruthy();

    // The Node-echoed route is the only thing the strip is fed. The Hub
    // validates it against the request and projects a matching echo only.
    await expect
      .poll(
        async () => {
          const record = await page.request.get(`/v1/instances/${instanceId}`);
          const body = (await record.json()) as {
            apiRoute?: { mode?: string; route?: string | null; viaHostId?: string | null } | null;
          };
          return body.apiRoute ?? null;
        },
        { timeout: 20_000 },
      )
      .toEqual({ mode: "via", route: "hub-relay", viaHostId: proxyId, viaHostLabel: "e2e-via-host" });

    await page.goto(`/s/${instanceId}/structured`);
    await expect(page.getByTestId("session-page")).toBeVisible();
    const clause = page.getByTestId("session-api-route");
    await expect(clause).toBeAttached();
    // The strip clause is readable operator text, not a host id or the
    // requested value.
    await expect(clause).toHaveText("经 e2e-via-host Hub 中转");
    expect(clause).toHaveAttribute("data-mode", "via");
    expect(clause).toHaveAttribute("data-route", "hub-relay");
    // Before the drop there is no error banner and no reroute clause.
    expect(page.getByTestId("session-api-route-down")).toHaveCount(0);

    // The clause lives inside the collapsed run-details disclosure; open it
    // for the evidence shot.
    await page.getByTestId("run-details-summary").click();
    await expect(clause).toBeVisible();
    await shot(page, "api-route-strip.png");

    // Drop the proxy host link. The Hub blocks the via worker and publishes
    // the api_route_down diagnostic; the strip must surface it as an error
    // while STILL naming the via route — never a changed route. The gate is a
    // port-scoped temp file the fake Node polls (same convention as the
    // tty-budget rpc gate).
    await mkdir(path.dirname(downGate), { recursive: true });
    await writeFile(downGate, "down\n");

    const down = page.getByTestId("session-api-route-down");
    await expect(down).toBeVisible({ timeout: 30_000 });
    expect(down).toHaveAttribute("role", "alert");
    expect(down).toContainText("api-route-down");
    // The route clause did not reroute: the error names the same via route.
    expect(page.getByTestId("session-api-route")).toHaveText("经 e2e-via-host Hub 中转");
    expect(page.getByTestId("session-api-route")).toHaveAttribute("data-down", "1");
    await shot(page, "api-route-down.png");

    // Lifting the gate reconnects the proxy; a reconnect never silently
    // clears the block (the operator re-dispatches), so the error remains
    // until then.
    await rm(downGate, { force: true });
    await expect(page.getByTestId("session-api-route-down")).toBeVisible({ timeout: 5_000 });
  } finally {
    await rm(downGate, { force: true });
    if (instanceId) {
      await page.request
        .delete(`/v1/instances/${instanceId}?force=1`, { headers })
        .catch(() => undefined);
    }
    await page.request.delete(`/v1/providers/${profileId}`, { headers }).catch(() => undefined);
  }
});
