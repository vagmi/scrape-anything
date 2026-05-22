# Luma Scraper (Chrome MV3)

Scrapes event details and event listings from lu.ma pages, then downloads them as JSON or CSV.

## Install (unpacked)

1. Open `chrome://extensions`.
2. Toggle **Developer mode** (top right).
3. Click **Load unpacked** and select this folder.

## Use

1. Navigate to a lu.ma page:
   - A single event: `https://lu.ma/<event-slug>` — yields one normalized event record.
   - A listing/calendar/discover page — yields all event links found in the grid.
2. Click the extension icon.
3. Click **Scrape this page**.
4. Click **Download JSON** or **Download CSV**.

## How it works

`content.js` is injected on demand via `chrome.scripting.executeScript`. It tries three sources in order:

1. **`__NEXT_DATA__`** — Luma is a Next.js app and embeds the full page payload as JSON. Cleanest source when present.
2. **JSON-LD** (`<script type="application/ld+json">`) — single event pages embed a schema.org `Event` object.
3. **DOM selectors** — fallback for anchors, titles, images on listing pages.

The two sources are merged so the result includes both schema.org fields (e.g. `priceCurrency`) and Luma-specific fields (e.g. `api_id`, `slug`).

## Files

- `manifest.json` — MV3 manifest, scoped to `https://lu.ma/*`.
- `popup.html` / `popup.css` / `popup.js` — UI and download logic.
- `content.js` — page scraping, returns a structured result.
- `background.js` — empty service worker, reserved for future use.

## Notes

- The extension only requests `activeTab`, `scripting`, and `downloads` permissions, plus `https://lu.ma/*` host access.
- Listing detection skips multi-segment paths (e.g. `/discover/tech`), which removes most non-event links.
- CSV output flattens nested values via `JSON.stringify` so they survive a round trip.
