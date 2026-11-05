// Native-only capability injected through the shell config.
(count) => window.__TAURI_INTERNALS__.invoke("plugin:window|set_badge_count", { label: "main", count })
