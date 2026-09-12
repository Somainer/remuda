import { readSession } from "./session";

const KEY = "runtime.access-code";

export function readAccessCode(): string {
  try {
    return localStorage.getItem(KEY) ?? "";
  } catch {
    return "";
  }
}

export function writeAccessCode(code: string): void {
  try {
    if (!code) localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, code);
  } catch {
    /* ignore */
  }
}

export function accessHeaders(): Record<string, string> {
  const code = readAccessCode();
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (code) {
    headers["X-Remuda-Access-Code"] = code;
    headers.Authorization = `Bearer ${code}`;
  }
  const session = readSession();
  if (session?.token) headers.Authorization = `Bearer ${session.token}`;
  return headers;
}
