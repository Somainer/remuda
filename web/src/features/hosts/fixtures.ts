import type { Id } from "../../types/wire";
import type { HostCli, HostView } from "./model";

/** Mock ~/.ssh/config aliases (proposal.md §4.4 / §4.6). */
export const SSH_ALIASES: { alias: string; hostname: string; user: string }[] = [
  { alias: "devbox", hostname: "devbox", user: "operator" },
  { alias: "devbox-sg", hostname: "devbox-sg", user: "operator" },
  { alias: "devbox-sg-host", hostname: "devbox-sg-host", user: "operator" },
  { alias: "devbox-sg-small", hostname: "devbox-sg-small", user: "operator" },
  { alias: "forge-doloris", hostname: "forge-doloris", user: "operator" },
  { alias: "devbox-gpu", hostname: "devbox-gpu", user: "operator" },
  { alias: "lyre-devbox", hostname: "lyre-devbox", user: "operator" },
];

const claudeCli = (auth: HostCli["auth"] = "unknown"): HostCli => ({
  kind: "claude",
  version: "2.1.268",
  path: "/opt/claude/bin/claude",
  auth,
});

/**
 * The three states of the `computer-use` row (D-045 §3.4), so a fixture-backed
 * view can exercise "yes", "no" and "not reported" side by side.
 */
const COMPUTER_USE_CLIENT =
  "computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient";

/** A macOS host that has the vendor client. */
const computerUseInstalled: HostCli = {
  kind: "computer-use",
  version: "2.7.0",
  path: `/opt/codex/${COMPUTER_USE_CLIENT}`,
  auth: "unknown",
  installed: true,
};

/** A host whose Node looked and did not find it. */
const computerUseAbsent: HostCli = { kind: "computer-use", auth: "unknown", installed: false };

export const HOST_FIXTURES: HostView[] = [
  {
    id: "hst_01993ab0-0000-7000-8000-00000000b001" as Id,
    label: "devbox-sg",
    state: "online",
    online: true,
    transport: "outbound-wss",
    lastSeenAt: "2026-09-12T00:00:00.000Z",
    rttMs: 12,
    agentVersion: "0.1.0",
    resources: { cpuPct: 8, memPct: 31 },
    cli: [
      claudeCli("logged_in"),
      { kind: "codex", version: "0.147.0", path: "/opt/codex/bin/codex", auth: "unknown" },
      { kind: "grok", version: "1.0.30", path: "/usr/local/bin/grok", auth: "unknown" },
      computerUseInstalled,
    ],
    labels: ["region:sg", "herdr", "gateway"],
    maxInstances: 8,
    instanceCount: 4,
    providerBinding: "auto",
    herdr: { version: "0.4.0", socket: "/tmp/herdr.sock" },
  },
  {
    id: "hst_01993ab0-0000-7000-8000-00000000b002" as Id,
    label: "devbox",
    state: "online",
    online: true,
    transport: "ssh-stdio",
    hostname: "devbox",
    lastSeenAt: "2026-09-12T00:00:00.000Z",
    rttMs: 48,
    agentVersion: "0.1.0",
    cli: [claudeCli("logged_out"), computerUseAbsent],
    labels: ["region:cn", "herdr"],
    maxInstances: 4,
    instanceCount: 1,
    providerBinding: "auto",
  },
  {
    id: "hst_01993ab0-0000-7000-8000-00000000b003" as Id,
    label: "runtime-local",
    state: "online",
    online: true,
    transport: "local",
    agentVersion: "0.1.0",
    rttMs: 1,
    cli: [claudeCli("logged_in")],
    labels: ["local"],
    maxInstances: 2,
    instanceCount: 0,
    providerBinding: "auto",
  },
  {
    id: "hst_01993ab0-0000-7000-8000-00000000b004" as Id,
    label: "forge-doloris",
    state: "offline",
    online: false,
    transport: "ssh-stdio",
    hostname: "forge-doloris",
    lastSeenAt: "2026-09-11T18:00:00.000Z",
    cli: [claudeCli("unknown")],
    labels: ["region:cn"],
    maxInstances: 2,
    instanceCount: 1,
    providerBinding: "auto",
  },
];
