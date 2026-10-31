import type { RootTurn } from "../types/generated";

/**
 * D-060 decision 1 (cb-rootturn): a small tag beside an initiator display
 * when the root turn an agent acted in was not started by the owner.
 *
 * Provenance only. The Hub records `rootTurn` and never gates on it, so the
 * tag renders quietly and nothing else on the surface changes with it. It is
 * absent when the turn is unknown or an owner's input started it.
 *
 * `surface` picks the wording: "Main 自主" where Main's own actions are
 * listed, "agent 自发" on `/approvals` (owner ruling R2).
 */
export type RootTurnSurface = "main" | "approvals";

const CONTINUATION_TEXT: Record<RootTurn["continuation"], string> = {
  none: "由这条输入开启",
  native: "agent 在没有新输入时自行续跑",
  hub: "由 Hub 送达的消息开启",
  unknown: "该会话不报告回合边界，按最近一条输入记录",
};

const ORIGIN_TEXT: Record<RootTurn["origin"], string> = {
  human: "你",
  bot: "bot",
  agent: "agent",
};

/** The tag text for `rootTurn`, or null when no tag is shown. */
export function rootTurnTagText(
  rootTurn: RootTurn | null | undefined,
  surface: RootTurnSurface,
): string | null {
  if (!rootTurn || rootTurn.origin === "human") return null;
  return surface === "approvals" ? "agent 自发" : "Main 自主";
}

/** Hover text: who started the turn and how exact the record is. */
export function rootTurnTitle(rootTurn: RootTurn): string {
  return `这一回合不是由你发起的：来源 ${ORIGIN_TEXT[rootTurn.origin]}，${CONTINUATION_TEXT[rootTurn.continuation]}（${rootTurn.commandId}）。仅作记录，不影响授权。`;
}
