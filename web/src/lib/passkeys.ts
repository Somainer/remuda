// WebAuthn passkey ceremony wrapper (D-029).
//
// The Hub speaks the webauthn-rs JSON dialect: challenge envelopes carry
// `{ publicKey: ... }` options whose binary fields (challenge, user.id,
// credential ids) are unpadded base64url, and finish calls take the browser's
// PublicKeyCredential with its binary response fields encoded the same way.
// This module owns that encoding plus feature detection and error
// normalization; nothing else in the app touches navigator.credentials.

export type PasskeyErrorKind = "unsupported" | "cancelled" | "unavailable" | "invalid";

export class PasskeyError extends Error {
  readonly kind: PasskeyErrorKind;

  constructor(kind: PasskeyErrorKind, message: string) {
    super(message);
    this.name = "PasskeyError";
    this.kind = kind;
  }
}

/** User-facing copy for each failure kind. */
export function passkeyErrorText(err: unknown): string {
  if (err instanceof PasskeyError) {
    switch (err.kind) {
      case "unsupported":
        return "此环境不支持 Passkey：非安全来源或浏览器过旧，请改用访问码登录。";
      case "cancelled":
        return "已取消 Passkey 验证。";
      case "unavailable":
        return "当前没有可用的 Passkey 或验证设备未响应。";
      case "invalid":
        return "Passkey 数据无效，请重试。";
    }
  }
  return err instanceof Error ? err.message : "Passkey 验证失败";
}

// --- base64url -------------------------------------------------------------

export function base64UrlEncode(bytes: BufferSource): string {
  const view = new Uint8Array(bytes instanceof ArrayBuffer ? bytes : bytes.buffer);
  let binary = "";
  for (let i = 0; i < view.length; i += 1) binary += String.fromCharCode(view[i] ?? 0);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function base64UrlDecode(text: string): Uint8Array<ArrayBuffer> {
  const normalized = text.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
  const binary = atob(padded);
  const buffer = new ArrayBuffer(binary.length);
  const bytes = new Uint8Array(buffer);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

// --- feature detection ------------------------------------------------------

type PublicKeyCredentialCtor = typeof PublicKeyCredential & {
  isConditionalMediationAvailable?: () => Promise<boolean>;
};

export function passkeysSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof navigator !== "undefined" &&
    typeof window.PublicKeyCredential === "function" &&
    (window.isSecureContext === true ||
      // Defensive: older engines lack isSecureContext; the API existing is
      // itself gated on a secure (or loopback) context.
      window.isSecureContext === undefined)
  );
}

export async function conditionalMediationAvailable(): Promise<boolean> {
  if (!passkeysSupported()) return false;
  const ctor = window.PublicKeyCredential as PublicKeyCredentialCtor;
  if (typeof ctor.isConditionalMediationAvailable !== "function") return false;
  try {
    return await ctor.isConditionalMediationAvailable();
  } catch {
    return false;
  }
}

// --- default label ----------------------------------------------------------

/**
 * Best-effort `Chrome on macOS` style label used as the default passkey name.
 * Falls back to platform/UA strings; the user can edit it before registering.
 */
export function defaultPasskeyName(userAgent: string = navigator.userAgent): string {
  const ua = userAgent;
  let os = "Unknown OS";
  if (/iPhone|iOS/i.test(ua)) os = "iPhone";
  else if (/iPad/i.test(ua)) os = "iPad";
  else if (/Mac OS X|Macintosh/i.test(ua)) os = "macOS";
  else if (/Windows NT/i.test(ua)) os = "Windows";
  else if (/Android/i.test(ua)) os = "Android";
  else if (/Linux|X11/i.test(ua)) os = "Linux";

  let browser = "Browser";
  // Check Edge/Opr before Chrome (they include "Chrome" in their UA).
  if (/Edg\//.test(ua)) browser = "Edge";
  else if (/OPR\/|Opera/.test(ua)) browser = "Opera";
  else if (/Chrome\//.test(ua) && !/Chromium/.test(ua)) browser = "Chrome";
  else if (/Chromium\//.test(ua)) browser = "Chromium";
  else if (/Firefox\//.test(ua)) browser = "Firefox";
  else if (/^((?!chrome|android).)*safari/i.test(ua)) browser = "Safari";

  return `${browser} on ${os}`;
}

// --- wire types -------------------------------------------------------------

export interface ServerCreationOptions {
  publicKey: {
    challenge: string;
    user: { id: string };
    excludeCredentials?: { id: string }[];
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

export interface ServerRequestOptions {
  publicKey: {
    challenge: string;
    allowCredentials?: { id: string }[];
    [key: string]: unknown;
  };
  mediation?: "conditional";
  [key: string]: unknown;
}

export interface RegisterAttestationBody {
  id: string;
  rawId: string;
  type: string;
  response: {
    attestationObject: string;
    clientDataJSON: string;
    transports?: string[];
  };
}

export interface LoginAssertionBody {
  id: string;
  rawId: string;
  type: string;
  response: {
    authenticatorData: string;
    clientDataJSON: string;
    signature: string;
    userHandle: string | null;
  };
}

// --- ceremonies -------------------------------------------------------------

function requireCredentials(): CredentialsContainer {
  if (!passkeysSupported() || !navigator.credentials) {
    throw new PasskeyError("unsupported", "WebAuthn is unavailable in this context");
  }
  return navigator.credentials;
}

function normalizeCeremonyError(err: unknown): PasskeyError {
  if (err instanceof PasskeyError) return err;
  const name = err instanceof DOMException ? err.name : "";
  if (name === "NotAllowedError" || name === "AbortError") {
    return new PasskeyError("cancelled", "Passkey ceremony dismissed");
  }
  if (name === "SecurityError") {
    return new PasskeyError("invalid", "SecurityError during passkey ceremony");
  }
  if (name === "NotSupportedError" || name === "InvalidStateError") {
    return new PasskeyError("unavailable", "No eligible passkey/credential");
  }
  return new PasskeyError("unavailable", err instanceof Error ? err.message : "ceremony failed");
}

function asBufferSource(value: unknown, field: string): BufferSource {
  if (typeof value === "string") {
    return base64UrlDecode(value);
  }
  throw new PasskeyError("invalid", `challenge option field ${field} is not base64url`);
}

/** Run navigator.credentials.create against a server register envelope. */
export async function createPasskey(options: ServerCreationOptions): Promise<RegisterAttestationBody> {
  const credentials = requireCredentials();
  const publicKey: PublicKeyCredentialCreationOptions = {
    ...options.publicKey,
    challenge: asBufferSource(options.publicKey.challenge, "challenge"),
    user: {
      ...options.publicKey.user,
      id: asBufferSource(options.publicKey.user.id, "user.id"),
    },
    excludeCredentials: options.publicKey.excludeCredentials?.map((descriptor) => ({
      ...descriptor,
      id: asBufferSource(descriptor.id, "excludeCredentials[].id") as BufferSource,
    })),
  } as PublicKeyCredentialCreationOptions;

  let credential: PublicKeyCredential;
  try {
    credential = (await credentials.create({ publicKey })) as PublicKeyCredential;
  } catch (err) {
    throw normalizeCeremonyError(err);
  }
  if (!credential) throw new PasskeyError("unavailable", "authenticator returned no credential");
  const response = credential.response as AuthenticatorAttestationResponse;
  return {
    id: credential.id,
    rawId: base64UrlEncode(credential.rawId),
    type: credential.type,
    response: {
      attestationObject: base64UrlEncode(response.attestationObject),
      clientDataJSON: base64UrlEncode(response.clientDataJSON),
      transports: typeof response.getTransports === "function" ? response.getTransports() : undefined,
    },
  };
}

/**
 * Run navigator.credentials.get. Default mediation is "required" (explicit
 * button); "conditional" is the autofill flow and stays pending until the
 * user picks a passkey from an autofill suggestion.
 */
export async function getPasskey(
  options: ServerRequestOptions,
  mediation: CredentialMediationRequirement = "required",
  signal?: AbortSignal,
): Promise<LoginAssertionBody> {
  const credentials = requireCredentials();
  const publicKey: PublicKeyCredentialRequestOptions = {
    ...options.publicKey,
    challenge: asBufferSource(options.publicKey.challenge, "challenge"),
    allowCredentials: (options.publicKey.allowCredentials ?? []).map((descriptor) => ({
      ...descriptor,
      id: asBufferSource(descriptor.id, "allowCredentials[].id") as BufferSource,
    })),
  } as PublicKeyCredentialRequestOptions;

  let credential: PublicKeyCredential;
  try {
    credential = (await credentials.get({ mediation, publicKey, signal })) as PublicKeyCredential;
  } catch (err) {
    throw normalizeCeremonyError(err);
  }
  if (!credential) throw new PasskeyError("unavailable", "authenticator returned no credential");
  const response = credential.response as AuthenticatorAssertionResponse;
  return {
    id: credential.id,
    rawId: base64UrlEncode(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: base64UrlEncode(response.authenticatorData),
      clientDataJSON: base64UrlEncode(response.clientDataJSON),
      signature: base64UrlEncode(response.signature),
      userHandle: response.userHandle ? base64UrlEncode(response.userHandle) : null,
    },
  };
}
