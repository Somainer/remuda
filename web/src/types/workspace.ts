import type { EntityMeta, Id, Knowledge } from "./wire";

export type RegisteredWorkspace = { workspaceId: string; hostId: string; root: string };
export type WorkspaceSnapshot = { hostId: string; workspaceRevision: number; workspaces: RegisteredWorkspace[] };

export type Workspace = EntityMeta & {
  hostId: Id;
  label: string;
  rootPath: string;
  writePolicy: string;
  canonicalRoot: Knowledge<string>;
  worktreeLabel?: string;
  branch?: string;
};
