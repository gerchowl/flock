`start.txt` preserves the text and particle positions captured in #2990, with
its worktree path replaced by a fictional fixture directory.

`send.txt` reconstructs the controls described in the issue comment, including
an idle composer, a background terminal and the model/shortcut footer. The
comment did not include particle positions or a full capture; the particles
here exercise the decoration captured in the issue body in that footer layout.

`harness.py` runs only inside the isolated delegate test server. It paints the
fixtures, records submitted lines, and accepts explicit stage commands so tests
can check draft refusals without model access or timed screen transitions.

`quoted-hint-draft.txt` and `multiline-braille-draft.txt` are the reviewer's
verified non-empty composers. `hint-tail-draft.txt` adds a Braille draft whose
last continuation line is the exact empty-composer hint. All three must be
preserved byte for byte and refused before input is written.
