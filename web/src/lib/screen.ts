export type ScreenRead = {
  lines: string[];
};

const DONE_LINE = /^\s*DONE(?:\s|$)/;

export function lastLines(lines: string[], n = 3): string[] {
  return lines.map((line) => line.replace(/\s+$/g, "")).filter((line) => line.length > 0).slice(-n);
}

export function doneFromLines(lines: string[]): boolean {
  return lines.some((line) => DONE_LINE.test(line));
}

export function parseScreenBody(body: unknown): ScreenRead {
  if (!body || typeof body !== "object") return { lines: [] };
  const rec = body as { lines?: unknown; text?: unknown };
  if (Array.isArray(rec.lines)) {
    return { lines: rec.lines.filter((line): line is string => typeof line === "string") };
  }
  if (typeof rec.text === "string") {
    return { lines: rec.text.split(/\r?\n/) };
  }
  return { lines: [] };
}
