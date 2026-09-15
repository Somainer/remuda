import type { Id } from "../../types/wire";
import type { HostCli, HostView } from "./model";

/** Mock ~/.ssh/config aliases (proposal.md §4.4 / §4.6). */
export const SSH_ALIASES: { alias: string; hostname: string; user: string }[] = [
  { alias: "devbox", hostname: "devbox", user: "devuser" },
  { alias: "devbox-sg", hostname: "devbox-sg", user: "devuser" },
  { alias: "devbox-sg-host", hostname: "devbox-sg-host", user: "devuser" },
  { alias: "devbox-sg-small", hostname: "devbox-sg-small", user: "devuser" },
  { alias: "forge-doloris", hostname: "forge-doloris", user: "devuser" },
  { alias: "devbox-gpu", hostname: "devbox-gpu", user: "devuser" },
  { alias: "lyre-devbox", hostname: "lyre-devbox", user: "devuser" },
];

const claudeCli = (auth: HostCli["auth"] = "unknown"): HostCli => ({
  kind: "claude",
  version: "2.1.268",
  path: "/home/devuser/.local/bin/claude",
  auth,
});

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
      { kind: "codex", version: "0.147.0", path: "/home/devuser/.local/bin/codex", auth: "unknown" },
      { kind: "grok", version: "1.0.30", path: "/usr/local/bin/grok", auth: "unknown" },
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
    cli: [claudeCli("logged_out")],
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
