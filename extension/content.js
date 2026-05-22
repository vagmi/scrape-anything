// Runs in the page context of lu.ma when the popup asks for a scrape.
// Strategy:
//   1. Pull __NEXT_DATA__ JSON if present — Luma is a Next.js app and most of
//      what's rendered comes from there. Cleanest source.
//   2. Pull schema.org JSON-LD blocks — single events embed an Event object.
//   3. Fall back to DOM selectors for listing pages and as a last resort.

(function () {
  function readNextData() {
    const el = document.getElementById("__NEXT_DATA__");
    if (!el) return null;
    try {
      return JSON.parse(el.textContent);
    } catch {
      return null;
    }
  }

  function readJsonLd() {
    const blocks = document.querySelectorAll('script[type="application/ld+json"]');
    const out = [];
    for (const b of blocks) {
      try {
        const parsed = JSON.parse(b.textContent);
        if (Array.isArray(parsed)) out.push(...parsed);
        else out.push(parsed);
      } catch {
        // skip malformed blocks
      }
    }
    return out;
  }

  function pageKind() {
    const path = location.pathname;
    if (path === "/" || path.startsWith("/discover") || path.startsWith("/home"))
      return "listing";
    // /<calendar-slug> with events grid, or /<event-slug> with a single event.
    // The presence of an Event JSON-LD block is the clearest signal.
    const jsonLd = readJsonLd();
    const hasEvent = jsonLd.some((o) => o && (o["@type"] === "Event" || o["@type"] === "SocialEvent"));
    if (hasEvent) return "event";
    return "listing";
  }

  function pickEventFromJsonLd(jsonLd) {
    return jsonLd.find((o) => o && (o["@type"] === "Event" || o["@type"] === "SocialEvent"));
  }

  function normalizeEvent(raw, sourceUrl) {
    if (!raw) return null;
    const loc = raw.location || {};
    const offer = raw.offers || {};
    const organizer = raw.organizer || {};
    return {
      url: sourceUrl || location.href,
      name: raw.name || null,
      description: raw.description || null,
      start: raw.startDate || null,
      end: raw.endDate || null,
      timezone: raw.timezone || null,
      event_status: raw.eventStatus || null,
      attendance_mode: raw.eventAttendanceMode || null,
      image: Array.isArray(raw.image) ? raw.image[0] : raw.image || null,
      location_name: loc.name || null,
      location_address:
        typeof loc.address === "string"
          ? loc.address
          : loc.address
          ? [loc.address.streetAddress, loc.address.addressLocality, loc.address.addressRegion, loc.address.addressCountry]
              .filter(Boolean)
              .join(", ")
          : null,
      organizer_name: organizer.name || null,
      organizer_url: organizer.url || null,
      price: offer.price ?? null,
      price_currency: offer.priceCurrency ?? null,
      tickets_url: offer.url ?? null,
    };
  }

  function scrapeSingleEvent() {
    const jsonLd = readJsonLd();
    const eventLd = pickEventFromJsonLd(jsonLd);
    let event = normalizeEvent(eventLd);

    // Augment with __NEXT_DATA__ fields when available.
    const next = readNextData();
    if (next) {
      const initial = next?.props?.pageProps?.initialData?.data || {};
      const e = initial.event || initial;
      if (e && typeof e === "object") {
        event = event || { url: location.href };
        event.api_id = e.api_id || event.api_id || null;
        event.slug = e.slug || event.slug || null;
        if (!event.name && e.name) event.name = e.name;
        if (!event.start && e.start_at) event.start = e.start_at;
        if (!event.end && e.end_at) event.end = e.end_at;
        if (!event.timezone && e.timezone) event.timezone = e.timezone;
        if (!event.description && e.description) event.description = e.description;
        if (!event.image && e.cover_url) event.image = e.cover_url;
        if (e.geo_address_info && !event.location_address) {
          const g = e.geo_address_info;
          event.location_address = [g.address, g.city_state, g.country].filter(Boolean).join(", ");
        }
      }
    }

    // DOM fallback for the bare minimum.
    if (!event) {
      const title = document.querySelector("h1")?.textContent?.trim();
      if (title) {
        event = { url: location.href, name: title };
      }
    }

    return event ? [event] : [];
  }

  function scrapeListing() {
    // Luma listing pages are anchor-grids. We collect every link whose pathname
    // looks like an event slug and dedupe by href.
    const anchors = document.querySelectorAll('a[href^="/"], a[href^="https://lu.ma/"]');
    const seen = new Map();
    for (const a of anchors) {
      let href;
      try {
        href = new URL(a.getAttribute("href"), location.origin).href;
      } catch {
        continue;
      }
      const u = new URL(href);
      if (u.hostname !== "lu.ma") continue;
      // Skip obvious non-event paths.
      const path = u.pathname.replace(/^\/+|\/+$/g, "");
      if (!path) continue;
      if (
        path.startsWith("discover") ||
        path.startsWith("home") ||
        path.startsWith("signin") ||
        path.startsWith("signup") ||
        path.startsWith("settings") ||
        path.startsWith("create") ||
        path.startsWith("help") ||
        path.startsWith("pricing") ||
        path.startsWith("about") ||
        path.includes("/")
      ) {
        // Skip multi-segment paths — event slugs are single-segment.
        continue;
      }
      if (seen.has(href)) continue;

      // Try to grab a card title from nearby text.
      const card = a.closest("[class*='card'], [class*='Card'], li, article") || a;
      const title =
        card.querySelector("h1, h2, h3")?.textContent?.trim() ||
        a.getAttribute("aria-label") ||
        a.textContent?.trim() ||
        null;
      const img = card.querySelector("img")?.src || null;
      const timeEl = card.querySelector("time");
      const start = timeEl?.getAttribute("datetime") || timeEl?.textContent?.trim() || null;

      seen.set(href, {
        url: href,
        slug: path,
        name: title,
        start,
        image: img,
      });
    }

    // Try to enrich from __NEXT_DATA__ when it carries an events array.
    const next = readNextData();
    if (next) {
      const queue = [next?.props?.pageProps];
      const collected = [];
      while (queue.length) {
        const node = queue.shift();
        if (!node || typeof node !== "object") continue;
        if (Array.isArray(node)) {
          for (const item of node) {
            if (item && item.event && item.event.url) collected.push(item.event);
            else if (item && item.api_id && (item.url || item.slug)) collected.push(item);
            else queue.push(item);
          }
          continue;
        }
        for (const v of Object.values(node)) queue.push(v);
      }
      for (const e of collected) {
        const href = e.url
          ? new URL(e.url, "https://lu.ma").href
          : e.slug
          ? `https://lu.ma/${e.slug}`
          : null;
        if (!href) continue;
        const existing = seen.get(href) || { url: href };
        existing.slug = existing.slug || e.slug || new URL(href).pathname.replace(/^\//, "");
        existing.name = existing.name || e.name || null;
        existing.start = existing.start || e.start_at || null;
        existing.end = e.end_at || null;
        existing.timezone = e.timezone || null;
        existing.image = existing.image || e.cover_url || null;
        seen.set(href, existing);
      }
    }

    return Array.from(seen.values());
  }

  function scrape() {
    const kind = pageKind();
    const items = kind === "event" ? scrapeSingleEvent() : scrapeListing();
    return {
      kind,
      source_url: location.href,
      scraped_at: new Date().toISOString(),
      count: items.length,
      items,
    };
  }

  // Return the result for chrome.scripting.executeScript to pick up.
  return scrape();
})();
