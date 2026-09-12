export class HubHttpError extends Error {
  readonly status: number;
  readonly code: string;
  readonly reasons: string[];

  constructor(status: number, code: string, message: string, reasons: string[] = []) {
    super(message);
    this.name = "HubHttpError";
    this.status = status;
    this.code = code;
    this.reasons = reasons;
  }
}

export function isUnauthorized(err: unknown): boolean {
  return err instanceof HubHttpError && err.status === 401;
}
