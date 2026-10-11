// Builds the structural fingerprint the backend planner consumes.
// SPEC §4. Pure-function side effect free except for reading the DOM.
// Installs window.__scrape.signals() so the panel can re-collect cheaply.

(function () {
  const VERSION = "0.2.0";
  const MAX_TOTAL_BYTES   = 240 * 1024;    // leave headroom under the 256 KB cap
  const MAX_NEXT_DATA     =  64 * 1024;
  const MAX_JSON_LD_TOTAL =  32 * 1024;
  const MAX_SAMPLE_BYTES  =   8 * 1024;
  const MAX_SAMPLES       =   3;
  const SKELETON_MAX_DEPTH    = 8;
  const SKELETON_MAX_SIBLINGS = 40;

  // CSS selectors we *try* against the page to give the LLM candidate roots
  // for repeating items. Counts only — no HTML.
  const CANDIDATE_SELECTORS = [
    "article",
    "li[class]",
    "[role='listitem']",
    "a[href][aria-label]",
    "a[href^='/']:has(time)",
    "a[href^='/']:has(img)",
    "[class*='card']",
    "[class*='Card']",
    "[data-testid]",
    "section li a[href^='/']",
  ];

  function readNextData() {
    const el = document.getElementById("__NEXT_DATA__");
    if (!el || !el.textContent) return null;
    try { return JSON.parse(el.textContent); } catch { return null; }
  }

  function readJsonLd() {
    const blocks = document.querySelectorAll('script[type="application/ld+json"]');
    const out = [];
    for (const b of blocks) {
      const txt = b.textContent;
      if (!txt) continue;
      try {
        const parsed = JSON.parse(txt);
        if (Array.isArray(parsed)) out.push(...parsed);
        else out.push(parsed);
      } catch { /* skip malformed */ }
    }
    return out;
  }

  function readMeta() {
    const out = {};
    out["title"] = document.title || null;
    for (const m of document.querySelectorAll("meta")) {
      const key = m.getAttribute("name") || m.getAttribute("property") || m.getAttribute("itemprop");
      const val = m.getAttribute("content");
      if (key && val && !(key in out)) out[key] = val;
    }
    // OpenGraph/Twitter live under property / name, both captured above.
    return out;
  }

  // Recursive structural skeleton. Strips text, keeps tag + id + class +
  // data-* keys. Counts children rather than listing them past the breadth cap.
  function buildSkeleton(el, depth) {
    if (!el || el.nodeType !== 1) return null;
    const node = { tag: el.tagName.toLowerCase() };
    if (el.id) node.id = el.id;
    const cls = el.getAttribute("class");
    if (cls) {
      const trimmed = cls.trim();
      if (trimmed) node.class = trimmed.length > 120 ? trimmed.slice(0, 120) + "…" : trimmed;
    }
    const dataKeys = [];
    for (const a of el.attributes) {
      if (a.name.startsWith("data-")) dataKeys.push(a.name);
    }
    if (dataKeys.length) node.data = dataKeys;

    const role = el.getAttribute("role");
    if (role) node.role = role;

    const text = el.firstChild && el.firstChild.nodeType === 3 ? el.firstChild.nodeValue : "";
    if (text && text.trim()) node.text_len = text.trim().length;

    const childEls = Array.from(el.children);
    node.n_children = childEls.length;

    if (depth < SKELETON_MAX_DEPTH && childEls.length > 0) {
      const kept = childEls.slice(0, SKELETON_MAX_SIBLINGS);
      const children = kept.map((c) => buildSkeleton(c, depth + 1)).filter(Boolean);
      if (childEls.length > kept.length) {
        children.push({ __truncated: true, omitted: childEls.length - kept.length });
      }
      node.children = children;
    }
    return node;
  }

  function probeCandidates() {
    const out = [];
    for (const sel of CANDIDATE_SELECTORS) {
      let count = 0;
      try { count = document.querySelectorAll(sel).length; } catch { continue; }
      if (count > 0) out.push({ selector: sel, count });
    }
    // Sort by count desc so the LLM sees the strongest signals first.
    out.sort((a, b) => b.count - a.count);
    return out;
  }

  function pickSamples(candidates) {
    const samples = [];
    for (const c of candidates) {
      if (samples.length >= MAX_SAMPLES) break;
      let nodes;
      try { nodes = document.querySelectorAll(c.selector); } catch { continue; }
      if (!nodes.length) continue;
      // Pick the most-substantial of the first few to avoid empty placeholders.
      let best = nodes[0];
      for (let i = 0; i < Math.min(5, nodes.length); i++) {
        if ((nodes[i].outerHTML?.length || 0) > (best.outerHTML?.length || 0)) {
          best = nodes[i];
        }
      }
      let html = best.outerHTML || "";
      if (html.length > MAX_SAMPLE_BYTES) {
        html = html.slice(0, MAX_SAMPLE_BYTES) + "\n<!-- __truncated -->";
      }
      samples.push({ selector: c.selector, html });
    }
    return samples;
  }

  // Size-aware truncation marker.
  function truncString(s, maxBytes) {
    if (typeof s !== "string") return s;
    if (s.length <= maxBytes) return s;
    return s.slice(0, maxBytes) + "\n…[__truncated]";
  }
  function truncJson(value, maxBytes) {
    const serialized = JSON.stringify(value);
    if (!serialized || serialized.length <= maxBytes) return value;
    return {
      __truncated: true,
      original_size: serialized.length,
      kept: "head",
      preview: serialized.slice(0, maxBytes) + "…",
    };
  }

  function collect() {
    const meta = readMeta();
    const jsonLd = readJsonLd();
    const nextData = readNextData();
    const candidates = probeCandidates();
    const samples = pickSamples(candidates);
    const skeleton = buildSkeleton(document.body, 0);

    let signals = {
      url: location.href,
      title: document.title || null,
      meta,
      json_ld: truncJson(jsonLd, MAX_JSON_LD_TOTAL),
      next_data: truncJson(nextData ?? {}, MAX_NEXT_DATA),
      skeleton,
      candidates,
      samples,
      viewport: { width: window.innerWidth, height: window.innerHeight },
      extension_version: VERSION,
    };

    // Final overall guard. If we're still over budget, shed the skeleton first
    // (samples + candidates are more informative for selector planning).
    let encoded = JSON.stringify(signals);
    if (encoded.length > MAX_TOTAL_BYTES) {
      signals.skeleton = { __truncated: true, original_size: encoded.length, kept: "dropped" };
      encoded = JSON.stringify(signals);
    }
    if (encoded.length > MAX_TOTAL_BYTES) {
      // Drop samples next.
      signals.samples = signals.samples.map((s) => ({
        selector: s.selector,
        html: truncString(s.html, 1024),
      }));
    }
    return signals;
  }

  window.__scrape = window.__scrape || {};
  window.__scrape.signals = collect;
  return collect();
})();
