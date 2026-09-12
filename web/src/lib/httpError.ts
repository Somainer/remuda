export class HubHttpError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "HubHttpError";
    this.status = status;
    this.code = code;
  }
}

export function isUnauthorized(err: unknown): boolean {
  return err instanceof HubHttpError && err.status === 401;
}
