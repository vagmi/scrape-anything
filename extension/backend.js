// Tiny HTTP client for the scrape-backend. Lives in the popup; no MV3
// service-worker dance needed because we only call localhost from a popup
// (extension origin → http://localhost works without host_permissions in MV3).

const DEFAULT_BASE = "http://localhost:4546";

function baseUrl() {
  // Future: read from chrome.storage.sync; for now a constant.
  return DEFAULT_BASE;
}

async function postJson(path, body) {
  const res = await fetch(baseUrl() + path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseResponse(res);
}

async function getJson(path) {
  const res = await fetch(baseUrl() + path);
  return parseResponse(res);
}

async function parseResponse(res) {
  const text = await res.text();
  let parsed;
  try { parsed = text ? JSON.parse(text) : null; } catch {
    throw new Error(`backend returned non-JSON (${res.status}): ${text.slice(0, 200)}`);
  }
  if (!res.ok) {
    const code = parsed?.error?.code || `HTTP_${res.status}`;
    const msg = parsed?.error?.message || res.statusText;
    const err = new Error(`${code}: ${msg}`);
    err.code = code;
    err.status = res.status;
    throw err;
  }
  return parsed;
}

export const backend = {
  createSession({ url, intention, signals }) {
    return postJson("/api/sessions", { url, intention, signals });
  },
  sendMessage(sessionId, content) {
    return postJson(`/api/sessions/${sessionId}/message`, { content });
  },
  recordResult(sessionId, payload) {
    return postJson(`/api/sessions/${sessionId}/result`, payload);
  },
  fetchSession(sessionId) {
    return getJson(`/api/sessions/${sessionId}`);
  },
  lookupPlan({ host, route, intention_hash }) {
    const q = new URLSearchParams({ host, route, intention_hash }).toString();
    return getJson(`/api/plans/lookup?${q}`);
  },
};
