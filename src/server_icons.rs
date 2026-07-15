//! Fleet server ICON registry (#164): maps a self-declared semantic icon NAME
//! to a flat Nerd Font glyph, resolved on the RENDERING side.
//!
//! A node declares `icon = "laptop"` in its config; only that ASCII name ever
//! crosses the wire (gossiped with the node's identity so every viewer renders
//! the same glyph). The name → glyph mapping lives here, on the receiver, so:
//!   - no raw/untrusted Unicode travels the wire (an unknown name renders no
//!     icon — [`glyph`] returns `None`);
//!   - the glyph is drawn from a font the RECEIVER controls (no cross-node font
//!     drift), and a registry bump reaches every node on next release.
//!
//! Codepoints are Material Design (`md-*`) / Font Awesome (`fa-*`) Nerd Font
//! glyphs, taken from the canonical `ryanoasis/nerd-fonts` glyphnames set.
//! Extending the fleet's icon vocabulary is a one-line addition here.

/// Resolve a self-declared icon NAME to its Nerd Font glyph. `None` for an
/// unknown name — the caller renders no icon (never the raw name), so a garbage
/// or version-skewed value from a peer can never reach the screen or the layout.
pub fn glyph(name: &str) -> Option<&'static str> {
    // Trim + lowercase so `"Laptop"` / ` laptop ` still resolve; names are
    // ASCII-semantic, never locale-sensitive.
    let key = name.trim().to_ascii_lowercase();
    let glyph = match key.as_str() {
        // — Devices & infrastructure —
        "laptop" => "\u{f0322}",
        "desktop" => "\u{f07c0}", // md-desktop_classic
        "monitor" => "\u{f0379}",
        "server" => "\u{f048b}",
        "database" | "db" => "\u{f01bc}",
        "nas" => "\u{f08f3}",
        "router" => "\u{f1087}", // md-router_network
        "disk" | "harddisk" => "\u{f02ca}",
        "memory" | "ram" => "\u{f035b}",
        "chip" => "\u{f061a}",
        "phone" | "cellphone" => "\u{f011c}",
        "tablet" => "\u{f04f6}",
        "pi" | "raspberrypi" | "raspberry_pi" => "\u{f043f}",
        "cloud" => "\u{f015f}",
        "console" | "terminal" => "\u{f018d}",
        "robot" => "\u{f06a9}",
        // — Operating systems —
        "apple" | "mac" => "\u{f0035}",
        "linux" => "\u{f033d}",
        "windows" => "\u{f05b3}", // md-microsoft_windows
        "penguin" => "\u{f0ec0}",
        // — Animals —
        "toad" | "frog" => "\u{edf8}", // fa-frog (md has no frog)
        "cat" => "\u{f011b}",
        "dog" => "\u{f0a43}",
        "fish" => "\u{f023a}",
        "owl" => "\u{f03d2}",
        "bird" => "\u{f15c6}",
        "turtle" => "\u{f0cd7}",
        "duck" => "\u{f01e5}",
        "snake" => "\u{f150e}",
        "bee" => "\u{f0fa1}",
        "ladybug" | "bug" => "\u{f082d}",
        "butterfly" => "\u{f1589}",
        "spider" => "\u{f11ea}",
        // — Tools —
        "anvil" => "\u{f089b}",
        "hammer" => "\u{f08ea}",
        "wrench" => "\u{f05b7}",
        "screwdriver" => "\u{f0476}",
        "axe" => "\u{f08c8}",
        "pickaxe" => "\u{f08b7}",
        "toolbox" => "\u{f09ac}",
        "tools" => "\u{f1064}",
        "cog" | "gear" => "\u{f0493}",
        // — Nature & misc —
        "tree" => "\u{f0531}",
        "leaf" => "\u{f032a}",
        "flower" => "\u{f024a}",
        "cactus" => "\u{f0db5}",
        "mushroom" => "\u{f07df}",
        "fire" => "\u{f0238}",
        "bolt" | "lightning" => "\u{f140b}",
        "star" => "\u{f04ce}",
        "heart" => "\u{f02d1}",
        "home" | "house" => "\u{f02dc}",
        "atom" => "\u{f0768}",
        "flask" => "\u{f0093}",
        "brain" => "\u{f09d1}",
        "ghost" => "\u{f02a0}",
        "alien" => "\u{f089a}",
        "skull" => "\u{f068c}",
        "crown" => "\u{f01a5}",
        "rocket" => "\u{f0463}",
        "ufo" => "\u{f10c4}",
        "shield" => "\u{f0498}",
        "anchor" => "\u{f0031}",
        "lightbulb" | "bulb" => "\u{f0335}",
        "bell" => "\u{f009a}",
        "flag" => "\u{f023b}",
        "diamond" => "\u{f01c8}", // md-diamond_stone
        "hexagon" => "\u{f02d8}",
        "cube" => "\u{f01a7}", // md-cube_outline
        // — Medical (e.g. ksb) —
        "hospital" => "\u{f02e1}",       // md-hospital_building
        "cross" | "plus" => "\u{f0415}", // md-plus (medical cross look)
        "medical" => "\u{f06ef}",        // md-medical_bag
        _ => return None,
    };
    Some(glyph)
}

/// Whether a name resolves to a glyph — used to warn on an unknown `icon` in a
/// node's OWN config (a typo), where a heads-up beats silent no-icon.
pub fn is_known(name: &str) -> bool {
    glyph(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_resolve_to_single_glyph() {
        for name in ["laptop", "toad", "anvil", "hammer", "hospital", "cross"] {
            let g = glyph(name).unwrap_or_else(|| panic!("{name} should resolve"));
            // Every registry glyph is exactly one char (a single Nerd Font
            // codepoint) — the render slot budgets one cell + a trailing space.
            assert_eq!(g.chars().count(), 1, "{name} glyph must be one codepoint");
        }
    }

    #[test]
    fn unknown_and_garbage_names_resolve_to_none() {
        // An unknown/typo'd name, and anything a hostile or version-skewed peer
        // could gossip, resolves to None — never rendered, never a raw string.
        assert_eq!(glyph("definitely-not-an-icon"), None);
        assert_eq!(glyph(""), None);
        assert_eq!(glyph("\u{1b}[31mred"), None); // escape-sequence payload → None
        assert!(!is_known("nope"));
    }

    #[test]
    fn aliases_and_trimming_resolve() {
        assert_eq!(glyph("frog"), glyph("toad"));
        assert_eq!(glyph("gear"), glyph("cog"));
        assert_eq!(glyph("  laptop  "), glyph("laptop"));
        assert_eq!(glyph("LAPTOP"), glyph("laptop"), "case-insensitive");
    }
}
