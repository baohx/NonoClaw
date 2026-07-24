const SENSITIVE_BROWSER_KEY = /(^|[_-])(api[_-]?key|authorization|credential|password|secret|token|prompt|attachment[_-]?data|extracted[_-]?text|images?|content|body)($|[_-])/i;
const SENSITIVE_BROWSER_TEXT = /(bearer\s+\S+|sk-(?:ant|proj)-\S+|api[_-]?key\s*[:=]|authorization\s*[:=]|password\s*[:=]|secret\s*[:=])/i;

export function sanitizeBrowserText(value: string): string {
  return SENSITIVE_BROWSER_TEXT.test(value) ? "[REDACTED]" : value;
}

/** Defensive browser-store filter. Server-side filtering remains canonical,
 * but malformed/legacy frames cannot persist credentials or attachment data. */
export function sanitizeBrowserValue(value: unknown, key = ""): unknown {
  if (SENSITIVE_BROWSER_KEY.test(key)) {
    if (key === "body") return "[skill content kept server-side]";
    if (key === "prompt_preview") return "[tool prompt hidden]";
    return undefined;
  }
  if (typeof value === "string") return sanitizeBrowserText(value);
  if (Array.isArray(value)) return value.map((item) => sanitizeBrowserValue(item)).filter((item) => item !== undefined);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(Object.entries(value as Record<string, unknown>)
    .map(([childKey, child]) => [childKey, sanitizeBrowserValue(child, childKey)] as const)
    .filter(([, child]) => child !== undefined));
}

export interface BrowserMediaAttachment {
  id: string;
  filename: string;
  extracted_text: string;
  image_count: number;
  images?: unknown[];
  uploading: boolean;
  error?: string;
}

export function sanitizeMediaAttachment<T extends BrowserMediaAttachment>(attachment: T): T {
  return {
    ...attachment,
    filename: sanitizeBrowserText(attachment.filename),
    extracted_text: "",
    images: undefined,
    error: attachment.error ? sanitizeBrowserText(attachment.error) : undefined,
  } as T;
}

export function sanitizeProjectInfo<T>(info: T): T {
  return sanitizeBrowserValue(info) as T;
}

let mobileAccessToken = "";

/** Keep the QR/mobile credential out of Zustand, localStorage, traces, and
 * serializable browser state. The token remains process-memory-only and is
 * exposed solely to authenticated networking and QR URL construction. */
export function setMobileAccessToken(value: unknown): boolean {
  mobileAccessToken = typeof value === "string"
    && value.length >= 16
    && value.length <= 128
    && /^[A-Za-z0-9_-]+$/.test(value)
    ? value
    : "";
  return mobileAccessToken.length > 0;
}

export function getMobileAccessToken(): string {
  return mobileAccessToken;
}

/** Prefer the credential explicitly present in the launch URL. After a direct
 * loopback bootstrap, reuse the token delivered by the authenticated info
 * frame so reconnects and HTTP APIs do not lose credentials. */
export function getBrowserAccessToken(search: string): string {
  return new URLSearchParams(search).get("token") || mobileAccessToken;
}

export function clearMobileAccessToken(): void {
  mobileAccessToken = "";
}
