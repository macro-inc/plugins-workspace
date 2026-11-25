---
"http": patch:perf
---

`fetch` no longer reads the webview's URL for every request to set `Origin`. Reading it waits for the UI thread, holding an async runtime worker until the UI thread answers; under load, requests waiting this way could stall every other async command. The plugin now records each webview's origin as its pages load (on both the start and the end of a load, so a request never carries the previous page's origin after a navigation to another one), reads the URL only for a webview no page load has been seen for, and forgets webviews whose window is destroyed. The header value is unchanged.
