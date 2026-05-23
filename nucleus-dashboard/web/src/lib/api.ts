// Typed fetch wrappers. One per backend route; grows as surfaces land.

export type Health = {
  status: string;
  service: string;
  version: string;
};

async function jsonGet<T>(path: string): Promise<T> {
  const res = await fetch(path);
  if (!res.ok) throw new Error(`${path} ${res.status}`);
  return res.json() as Promise<T>;
}

export const getHealth = () => jsonGet<Health>("/api/health");
