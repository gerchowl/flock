---
title: Workbench layout preview
description: Try the client tile manager on desktop and phone.
---

The fixture preview (`just client-preview`) shows a recorded transcript with a replay badge. Use the Subject, Tile kind and Placement controls to open views. Transcript fixtures serve `agent/fixture`; other streams show an explicit replay failure. Other tile kinds show a labelled empty view until their implementation slices ship.

Desktop layouts use weighted splits and stacks of tabs. Placement defaults to replacing the focused tile. Choose `tab`, `split_right` or `split_down` to keep the current tile. **Expand into second** opens the selected subject and kind beside its origin; closing that origin leaves the second tile open. Collapse moves a tile to the edge rail; select its chip to restore it. Close removes it permanently. Rail chips show the subject, any supplied live badge, and its last-change time.

Use Tab and Shift+Tab to reach every control, and Enter or Space to activate buttons. Alt+ArrowLeft and Alt+ArrowRight move focus between desktop tiles, including tabs hidden in stacks. Focusing a tile calls the inbox filter interface with that tile's subject.

Below **768 CSS pixels**, one tile is visible, a tile switcher replaces the splits, and the rail opens as a bottom sheet. Open defaults to `push`. Expand pushes a second tile; browser Back or the visible Back control returns to its origin. Explicit split placements retain the desktop tree while the phone shows only its focused tile. The same tree is restored when the viewport widens. Touch controls are at least 44 pixels and the sheet respects safe-area insets. Escape closes the sheet.

Append `?layout=phone` or `?layout=desktop` to force a mode for preview; omit it for automatic detection. Embedders and tests can instead pass `Workbench` a `mode` prop of `auto`, `phone` or `desktop`.

Layouts are stored locally on this device. Clearing browser storage resets the layout without affecting server state. An obsolete schema, corrupt tree or failed storage read resets the whole layout and displays a notice. A failed write displays a notice that changes last only for the current session. Layout storage includes subject references and cursors, so clearing it also clears those local presentation references.

`Workbench` requires a stable `serverKey`. Fixtures use the literal `fixture`; before a wire welcome, a shell may use the configured URL normalized to scheme, host, port and path, without credentials or token. G3 (#1595) supplies the real server identity after welcome. Live mode remains with G3; this preview does not connect to a server. Named, server-synchronized layouts are deferred by ADR-0038 owner decision 9.

The layout model's subscription callbacks are the integration boundary for G3 (#1595). G3 initializes subscriptions for loaded state and supplies cursors and badges. Collapsing a tile without a badge releases its subscription unless another tile shares it; restoring resumes from the saved cursor. A chip with a badge keeps its subscription. Closing releases a stream only after its last owning tile is removed. The fixture Transcript component manages its own replay subscriptions; it does not exercise G3's lifecycle or share a live stream store.
