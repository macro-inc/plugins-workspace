---
"http": minor:perf
---

Reuse one HTTP client for requests that set no client options (`connectTimeout`, `maxRedirections`, `proxy`, `danger`), so requests to a host share its connections instead of each opening a new one with its own TCP and TLS handshake. An HTTP/2 connection is probed when a request hears nothing for ten seconds and dropped if the probe goes unanswered for twenty. `Http::reset_connections` drops the pooled connections, for callers that know they may have died, and a request that fails to connect replaces the client so changed system proxy settings are read again.
