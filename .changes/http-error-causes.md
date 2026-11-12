---
"http": patch:bug
---

Include the underlying cause (DNS, connect, TLS) in the error a failed request rejects with, instead of only reqwest's "error sending request for url (…)".
