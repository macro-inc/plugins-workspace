---
"http": minor:perf
---

Reuse one HTTP client for requests that set no client options (`connectTimeout`, `maxRedirections`, `proxy`, `danger`), so requests to a host share its connections instead of each opening a new one with its own TCP and TLS handshake. An HTTP/2 connection is probed when a request hears nothing for five seconds and dropped if the probe goes unanswered for twenty, and its flow-control window is large enough that responses left unread cannot stall the other requests sharing it. `Http::reset_connections` drops the pooled connections, for callers that know they may have died, and the plugin does the same when the machine slept ten seconds or more since the last request, on platforms whose monotonic clock stops during sleep. On macOS and Windows a request that can no longer connect to an origin it reached before replaces the client, so that system proxy settings are read again where `reqwest` is built to read them.
