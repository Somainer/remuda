import type { EntityMeta, Id, Knowledge } from "./wire";

export type Workspace = EntityMeta & {
  hostId: Id;
  label: string;
  rootPath: string;
  writePolicy: string;
  canonicalRoot: Knowledge<string>;
  worktreeLabel?: string;
  branch?: string;
};
