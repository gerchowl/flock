// Native-only capability injected through the shell config.
(count) => window.__TAURI_INTERNALS__.invoke("set_badge_count", { count })
