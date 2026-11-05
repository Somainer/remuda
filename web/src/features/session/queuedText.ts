/** The prompt text of a queued send, in whichever shape it was sent. */
export function queuedText(payload: Record<string, unknown> | undefined): string {
  if (!payload) return "";
  const input = payload.input as Record<string, unknown> | string | undefined;
  if (typeof input === "string") return input;
  if (input && typeof input.text === "string") return input.text;
  const blocks = input && Array.isArray(input.blocks) ? input.blocks : [];
  const texts = blocks
    .filter((block): block is { type: string; text: string } =>
      Boolean(block) && (block as { type?: unknown }).type === "text" &&
      typeof (block as { text?: unknown }).text === "string")
    .map((block) => block.text);
  if (texts.length) return texts.join("\n");
  if (typeof payload.text === "string") return payload.text;
  if (typeof payload.prompt === "string") return payload.prompt;
  return "";
}
