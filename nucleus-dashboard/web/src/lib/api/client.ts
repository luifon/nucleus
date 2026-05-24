// Shared HTTP client primitives. One pair (`jsonGet` / `jsonPost`) used
// by every per-domain api file under this directory.
//
// Keep this small and provider-agnostic. Domain-specific concerns
// (caching, retry, optimistic mutation) belong in the per-domain
// modules — push them down only when a real surface needs them, never
// pre-emptively.

/** Thrown by `jsonGet` / `jsonPost` on non-2xx responses. Carries the
 *  HTTP status so callers can branch (e.g. 503 → "subsystem not
 *  initialized" instead of generic error UI). */
export class ApiError extends Error {
  constructor(
    public readonly path: string,
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function readErrorMessage(res: Response, path: string): Promise<string> {
  // Backend conventionally returns `{ "error": "..." }` for known
  // failure modes. Fall back to the status text if the body isn't
  // shaped that way.
  try {
    const body = (await res.json()) as { error?: unknown };
    if (body && typeof body.error === "string") return body.error;
  } catch {
    /* not JSON — fall through */
  }
  return `${path} → ${res.status} ${res.statusText}`;
}

export async function jsonGet<T>(path: string): Promise<T> {
  const res = await fetch(path);
  if (!res.ok) {
    throw new ApiError(path, res.status, await readErrorMessage(res, path));
  }
  return res.json() as Promise<T>;
}

export async function jsonPost<T, B>(path: string, body: B): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    throw new ApiError(path, res.status, await readErrorMessage(res, path));
  }
  return res.json() as Promise<T>;
}

/** URL-search-params builder with omit-undefined semantics. Most
 *  list endpoints take optional filters; this saves every domain
 *  module from re-implementing the same null check. */
export function qs(params: Record<string, string | number | boolean | undefined>): string {
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined) sp.set(k, String(v));
  }
  const s = sp.toString();
  return s ? `?${s}` : "";
}
