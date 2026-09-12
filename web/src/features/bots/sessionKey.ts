/** Feishu session_key from remuda-feishu README / inbound.rs. */
export function feishuSessionKey(chatId: string, threadId?: string | null, rootId?: string | null): string {
  const tail = threadId || rootId || "main";
  return `feishu:${chatId}:${tail}`;
}

export function telegramSessionKey(chatId: string, messageThreadId?: number | null): string {
  return `tg:${chatId}:${messageThreadId ?? 0}`;
}

export const FEISHU_SESSION_RULE = "feishu:{chat_id}:{thread_id||root_id||main}";
export const TELEGRAM_SESSION_RULE = "tg:{chat_id}:{message_thread_id||0}";
