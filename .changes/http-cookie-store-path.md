---
"http": minor:feat
---

Allow Rust callers to choose the persistent cookie file with `init_with_cookie_store_path`, including Tauri base-directory variables. The default cache location remains unchanged, and cookies are not migrated between locations.
