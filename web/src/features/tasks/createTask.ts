/**
 * Task creation with the directory binding selector (task-model t-bind).
 *
 * The binding is validated purely in `binding.ts`; this module only performs
 * the HTTP call and remembers the operator's choice per project (D2:
 * force-choose-then-remember). The response is the created task plus, for a
 * bound task, the `sharing` projection (refcount / serial-reuse queue).
 */
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import {
  type BindingChoice,
  buildTaskCreateBody,
  hasRememberedBinding,
  rememberBindingChoice,
} from "./binding";

type TaskCreate = components["schemas"]["TaskCreate"];
type TaskCreateResult = components["schemas"]["TaskCreateResult"];

export type CreateTaskInput = {
  projectId: string;
  title: string;
  intent: string;
  class?: TaskCreate["class"];
  owns?: string[];
  binding?: BindingChoice;
};

/**
 * POST /v1/tasks. When a binding is supplied the Hub acquires the lease as
 * part of creation; a pool-full / dirty-directory refusal comes back as the
 * usual HubHttpError (429 SUPPLY_DEFERRED / 409) and the task is not created.
 * A successful choice is remembered per project.
 */
export async function createTask(input: CreateTaskInput): Promise<TaskCreateResult> {
  const body = buildTaskCreateBody(input);
  const result = await rest<TaskCreateResult>("/v1/tasks", {
    method: "POST",
    body: JSON.stringify(body),
  });
  if (input.binding) {
    rememberBindingChoice(input.projectId, {
      mode: input.binding.mode,
      worktreeName: input.binding.worktreeName,
    });
  }
  return result;
}

/**
 * True once a project has a remembered binding mode: the UI may preselect it.
 * Before that, creation must force an explicit reuse|pool choice (D2).
 */
export function bindingChoiceKnown(projectId: string): boolean {
  return hasRememberedBinding(projectId);
}
