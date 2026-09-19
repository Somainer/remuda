import type { Instance } from "../../types/instance";
import type { UiStatus } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { projectStatus } from "../../lib/status";

/**
 * The single sentence under a list-row title (ui-spec.md D-038, §2.1
 * wireframe: 点 + 标题 + 一句下一步). It is a *projection* of fields the instance, its
 * pending interaction and its journal tail already carry — no new state
 * machine, no success inference:
 *  - `projectStatus` decides the six dots, and an unknown/disconnected row
 *    always reads as 状态待确认 even when its activity field claims work;
 *  - a pending interaction contributes its request summary;
 *  - a working row shows its live-run phrase (`summary`) when the journal
 *    tail produced one, and the constant 运行中… only when nothing is known;
 *  - an exited row promises 可恢复 only when the `resume` capability is
 *    actually supported;
 *  - the exit code and the screen DONE marker are the only settled signals,
 *    and neither turns a row into a success state.
 */
export type NextStepTone = UiStatus;

export type NextStep = {
  text: string;
  tone: NextStepTone;
};

export type RowScreen = { lines: string[]; done: boolean } | undefined;

function pendingSummary(pending: Interaction): string | null {
  const { request } = pending;
  switch (request.kind) {
    case "approval":
      return request.description.trim() || `等你批准 ${request.title}`.trim();
    case "question":
      return `${request.title} · ${request.fields.length} 题待回答`;
    case "plan-review":
      return request.title ? `计划待审 · ${request.title}` : "计划待审";
    case "elicitation":
      return request.title ? `待处理表单 · ${request.title}` : "待处理表单";
  }
}

export function nextStep(
  instance: Instance,
  pending?: Interaction | null,
  screen?: RowScreen,
  summary?: string,
): NextStep {
  const status = projectStatus(instance);

  // Connectivity/unknown wins before anything the turn claims: a
  // disconnected host never reads as idle or working.
  if (status === "unknown") {
    return { text: "状态待确认 · 不推断成功或结束", tone: "unknown" };
  }

  // A live interaction is the most actionable thing the row can say.
  if (pending && pending.state === "pending") {
    const text = pendingSummary(pending);
    if (text) return { text, tone: "blocked" };
  }
  if (status === "blocked") {
    return { text: "等待处理交互", tone: "blocked" };
  }

  if (status === "starting") {
    return { text: "正在拉起会话…", tone: "starting" };
  }
  if (status === "working") {
    // The live-run phrase is projected from the journal tail (workflow run +
    // phase, else the latest assistant line). With no known phrase the row
    // says the constant — never an invented status. The DONE marker is a
    // screen observation, not lifecycle: the dot stays `working`.
    const phrase = summary?.trim();
    if (screen?.done) {
      return {
        text: phrase ? `终端已打出 DONE · ${phrase}` : "终端已打出 DONE · 仍在运行",
        tone: "working",
      };
    }
    return { text: phrase || "运行中…", tone: "working" };
  }
  if (status === "idle") {
    return { text: "回合结束、进程仍在 · 可继续发送", tone: "idle" };
  }

  // exited: carry the exit code when it is known, and promise recovery only
  // when the instance actually advertises the resume capability (the session
  // page disables Resume otherwise, D-026).
  const code = instance.exit.state === "known" ? instance.exit.value.code : null;
  const codePart = code == null ? "会话已退出" : `会话已退出 · exit ${code}`;
  const canResume = instance.capabilities.capabilities.resume?.state === "supported";
  return {
    text: canResume ? `${codePart} · 可恢复` : codePart,
    tone: "exited",
  };
}
