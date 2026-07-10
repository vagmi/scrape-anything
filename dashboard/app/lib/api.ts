import type {
  Artifact,
  OriginFile,
  OriginFiles,
  SessionDetail,
  SessionListItem,
} from "./types";

// Same-origin in production (the Rust backend serves this SPA). In dev, Vite
// proxies /api → http://localhost:4546 (see vite.config.ts).
const BASE = "";

async function get<T>(path: string): Promise<T> {
  const res = await fetch(BASE + path);
  if (!res.ok) {
    throw new Response(await res.text(), { status: res.status });
  }
  return (await res.json()) as T;
}

export const api = {
  sessions: (q?: string) =>
    get<{ sessions: SessionListItem[] }>(
      `/api/sessions${q ? `?q=${encodeURIComponent(q)}` : ""}`,
    ),
  session: (id: string) => get<SessionDetail>(`/api/sessions/${id}`),
  artifact: (id: string) => get<Artifact>(`/api/sessions/${id}/artifact`),
  originFiles: (host: string) =>
    get<OriginFiles>(`/api/origins/${encodeURIComponent(host)}/files`),
  originFile: (host: string, path: string) =>
    get<OriginFile>(`/api/origins/${encodeURIComponent(host)}/files/${path}`),

  artifactDownloadUrl: (artifactId: string, fmt: "json" | "csv") =>
    `${BASE}/api/artifacts/${artifactId}.${fmt}`,
};

export function fmtTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}
