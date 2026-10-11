// The UI lives in a window-wide side panel rather than a popup: a popup closes
// the moment it loses focus, which killed the agent loop on every page click.
// The panel stays open across navigations and tab switches, so the loop it
// drives does too. Clicking the toolbar icon opens it.
chrome.sidePanel
  .setPanelBehavior({ openPanelOnActionClick: true })
  .catch((e) => console.error("sidePanel.setPanelBehavior failed", e));
