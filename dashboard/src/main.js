import "./styles.css";
import htmx from "htmx.org";

// Expose htmx globally and process the initial DOM (module scripts are deferred,
// so DOMContentLoaded may already have fired by the time we run).
window.htmx = htmx;
if (document.readyState !== "loading") {
  htmx.process(document.body);
}
