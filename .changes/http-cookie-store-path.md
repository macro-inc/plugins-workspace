---
"http": minor:feat
---

Allow Rust callers to choose the persistent cookie storage directory with `init_with_cookie_store_dir`, including Tauri base-directory variables. The plugin owns the `.cookies` filename. The default cache location remains unchanged, and cookies are not migrated between locations.
