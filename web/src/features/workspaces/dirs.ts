/**
 * c-dirpicker: human-only host directory browser, backed by the Node's
 * `host.dirs.list` RPC (Hub route `GET /v1/hosts/{id}/dirs`). The Node lists
 * directories only inside its configured workspace_roots and never follows
 * symlinks.
 */
export type HostDirEntry = { name: string };

export type HostDirsListing = {
  /** Canonical absolute directory listed. */
  path: string;
  /** Parent when still inside an allowed root; null at the boundary. */
  parent?: string | null;
  /** User home when inside an allowed root. */
  home?: string | null;
  /** Configured allowlist roots. */
  roots: string[];
  /** Already-registered workspace roots, for quick jumps. */
  workspaces: string[];
  /** Subdirectories, sorted, capped. */
  dirs: HostDirEntry[];
  /** True when entries were dropped at the cap. */
  truncated: boolean;
};

export type HostDirsQuery = {
  path?: string;
  showHidden?: boolean;
};
