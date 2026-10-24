// A session can span several sites, one tab per site, all kept together in a
// tab group named for the session. Navigating within a site reuses its tab;
// going to a different site opens (or switches to) that site's own tab. That
// way a half-filled form on one site isn't thrown away because the agent went
// off to read a page somewhere else — it's still sitting in its tab when the
// agent comes back.
//
// `ctx` is the session's mutable tab context, `{ tabId, groupId }`. `tabId` is
// the tab the tools currently act on; functions here move it when they switch.

const GROUP_COLOR = "purple";

/// Registrable domain, approximately: the last two labels, or three when the
/// second-to-last is a generic second-level label under a ccTLD (`co.uk`,
/// `com.au`). Good enough to tell sites apart without shipping the PSL.
export function baseDomain(url) {
  let host;
  try { host = new URL(url).hostname; } catch { return null; }
  if (!host.includes(".") || /^[\d.]+$/.test(host) || host.includes(":")) return host;
  const labels = host.split(".");
  const n = labels.length;
  const secondLevel = /^(co|com|net|org|gov|edu|ac|or|ne|go)$/;
  if (n >= 3 && labels[n - 1].length === 2 && secondLevel.test(labels[n - 2])) {
    return labels.slice(-3).join(".");
  }
  return labels.slice(-2).join(".");
}

async function groupExists(groupId) {
  if (groupId == null || groupId === -1) return false;
  try { await chrome.tabGroups.get(groupId); return true; } catch { return false; }
}

/// Put the session's current tab in the session's group, creating the group
/// (titled `title`) if there isn't one yet. Best-effort: a tab that can't be
/// grouped (pinned, say) just stays where it is.
export async function ensureGroup(ctx, title = "Scrape") {
  try {
    const tab = await chrome.tabs.get(ctx.tabId);
    if (await groupExists(ctx.groupId)) {
      if (tab.groupId !== ctx.groupId) {
        await chrome.tabs.group({ tabIds: [tab.id], groupId: ctx.groupId });
      }
      return;
    }
    ctx.groupId = await chrome.tabs.group({
      tabIds: [tab.id],
      createProperties: { windowId: tab.windowId },
    });
    await chrome.tabGroups.update(ctx.groupId, { title: groupTitle(title), color: GROUP_COLOR });
  } catch (e) {
    console.warn("could not group session tab:", e?.message || e);
  }
}

function groupTitle(text) {
  const t = (text || "").trim().replace(/\s+/g, " ");
  return t.length > 24 ? `${t.slice(0, 23)}…` : t || "Scrape";
}

/// The tabs in the session's group (just the current tab if there's no group).
export async function sessionTabs(ctx) {
  if (await groupExists(ctx.groupId)) {
    return await chrome.tabs.query({ groupId: ctx.groupId });
  }
  try { return [await chrome.tabs.get(ctx.tabId)]; } catch { return []; }
}

/// Where a navigation to `target` should happen. Returns
/// `{ tab, kind }` with kind "same" (the current tab), "existing" (another of
/// the session's tabs already on that site) or "new" (no tab yet — caller
/// opens one with `openTab`).
export async function tabForTarget(ctx, current, target) {
  const site = baseDomain(target);
  if (!current?.url || baseDomain(current.url) === site) return { tab: current, kind: "same" };
  const tabs = await sessionTabs(ctx);
  const match = tabs.find((t) => t.id !== current.id && t.url && baseDomain(t.url) === site);
  return match ? { tab: match, kind: "existing" } : { tab: null, kind: "new" };
}

/// Open `url` in a fresh tab next to `current`, add it to the session group,
/// and make it the session's current tab.
export async function openTab(ctx, current, url) {
  const tab = await chrome.tabs.create({
    url,
    windowId: current.windowId,
    index: current.index + 1,
    openerTabId: current.id,
    active: true,
  });
  ctx.tabId = tab.id;
  await ensureGroup(ctx);
  return tab;
}

/// Make `tab` the session's current tab and bring it to the front.
export async function switchTo(ctx, tab) {
  ctx.tabId = tab.id;
  await chrome.tabs.update(tab.id, { active: true });
}
