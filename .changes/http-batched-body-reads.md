---
"http": patch:perf
---

`fetch_read_body` returns every chunk of the body that has already arrived, up to 1 MiB, instead of one chunk per call, so a buffered body reaches the page in one IPC round trip rather than one per 4 KiB decoder chunk or HTTP/2 frame. A read still waits only for its first chunk and never for more, so a streamed body is delivered as it arrives. The wire format is unchanged: older JavaScript reads the larger chunks the same way. The end of the body or a body error found after a read has taken data is reported by the next read.
