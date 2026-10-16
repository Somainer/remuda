export class HubHttpError extends Error {
  readonly status: number;
  readonly code: string;
  readonly reasons: string[];
  /** Hub-supplied back-off for retryable refusals (503 NODE_BUSY), in ms. */
  readonly retryAfterMs?: number;

  constructor(status: number, code: string, message: string, reasons: string[] = [], retryAfterMs?: number) {
    super(message);
    this.name = "HubHttpError";
    this.status = status;
    this.code = code;
    this.reasons = reasons;
    if (typeof retryAfterMs === "number" && Number.isFinite(retryAfterMs)) {
      this.retryAfterMs = retryAfterMs;
    }
  }
}

export function isUnauthorized(err: unknown): boolean {
  // instanceof first (same module realm — production path), with a
  // structural fallback for cross-realm duplicates (e.g. vitest with
  // vi.resetModules(): a thrown HubHttpError from a separately evaluated
  // httpError module fails instanceof but still carries status===401).
  if (err instanceof HubHttpError) return err.status === 401;
  return (
    typeof err === "object" &&
    err !== null &&
    (err as { name?: unknown }).name === "HubHttpError" &&
    (err as { status?: unknown }).status === 401
  );
}
