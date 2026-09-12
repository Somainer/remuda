import type { Id } from "../../types/wire";
import { SSH_ALIASES } from "./fixtures";
import type { HostCli, HostView } from "./model";

export type ProbeResult =
  | { ok: true; alias: string; hostname: string; rttMs: number; cli: HostCli[] }
  | { ok: false; alias: string; error: string };

const CLAUDE: HostCli = {
  kind: "claude",
  version: "2.1.268",
  path: "/home/devuser/.local/bin/claude",
  auth: "unknown",
};

/** Mock Hub probe of an ssh-stdio alias. Live path is TODO(remuda-hub) host.probe. */
export async function probeSshAlias(alias: string): Promise<ProbeResult> {
  await wait(120);
  const row = SSH_ALIASES.find((a) => a.alias === alias);
  if (!row) return { ok: false, alias, error: "unknown ssh config alias" };
  if (alias === "forge-doloris") return { ok: false, alias, error: "GSSAPI failed: ssh config User field has a trailing comment" };
  if (alias === "devbox-sg-small") return { ok: false, alias, error: "ssh probe timed out" };
  return { ok: true, alias, hostname: row.hostname, rttMs: alias.endsWith("-sg") || alias.includes("sg") ? 12 : 40, cli: [CLAUDE] };
}

export type BootstrapStep = { id: string; label: string; state: "pending" | "running" | "done" | "failed" };

export function bootstrapPlan(alias: string): BootstrapStep[] {
  return [
    { id: "scp", label: `scp remuda-node → ${alias}`, state: "pending" },
    { id: "stdio", label: `ssh ${alias} remuda node --stdio`, state: "pending" },
    { id: "enroll", label: "host.report → enrolled", state: "pending" },
  ];
}

/** Mock ssh-stdio bootstrap. Live path is TODO(remuda-hub) host.bootstrap. */
export async function runBootstrap(
  alias: string,
  onStep: (steps: BootstrapStep[]) => void,
): Promise<HostView> {
  const probe = await probeSshAlias(alias);
  if (!probe.ok) throw new Error(probe.error);
  const steps = bootstrapPlan(alias);
  for (let i = 0; i < steps.length; i++) {
    steps[i] = { ...steps[i], state: "running" };
    onStep(steps.map((s) => ({ ...s })));
    await wait(90);
    steps[i] = { ...steps[i], state: "done" };
    onStep(steps.map((s) => ({ ...s })));
  }
  let n = 0;
  for (const ch of alias) n = (n * 33 + ch.charCodeAt(0)) >>> 0;
  const hex = n.toString(16).padStart(12, "0").slice(-12);
  return {
    id: `hst_01993ab0-0000-7000-8000-${hex}` as Id,
    label: alias,
    state: "online",
    online: true,
    transport: "ssh-stdio",
    hostname: probe.hostname,
    rttMs: probe.rttMs,
    agentVersion: "0.1.0",
    cli: probe.cli,
    labels: [],
    maxInstances: 4,
    instanceCount: 0,
    providerBinding: "auto",
  };
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => {
    window.setTimeout(resolve, ms);
  });
}
