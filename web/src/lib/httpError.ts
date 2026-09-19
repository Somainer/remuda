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
  return err instanceof HubHttpError && err.status === 401;
}
