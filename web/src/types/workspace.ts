import type { EntityMeta, Id, Knowledge } from "./wire";

export type RegisteredWorkspace = {
  workspaceId: string;
  hostId: string;
  root: string;
  /** Optional worktree metadata (the Workspace entity's `worktree` record);
   *  absent from the minimal Node registry reply, carried by richer clients. */
  worktreeLabel?: string;
  branch?: string;
};
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
