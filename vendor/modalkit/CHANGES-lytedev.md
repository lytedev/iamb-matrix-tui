# Local changes to modalkit 0.0.25

This directory holds the published source of modalkit 0.0.25, with one fix
applied. The root `Cargo.toml` of iamb selects this copy through
`[patch.crates-io]`.

Delete this directory and the `[patch.crates-io]` section when a modalkit
release contains the fix.

## Fix: `changenum` loops forever on a buffer that holds no number

`EditBuffer::changenum` in `src/editing/buffer/edit.rs` runs on `<C-A>` and
`<C-X>`. The loop advances the cursor only at the end of the body. Two
`continue` statements go past that advance:

- `get_cursor_word_mut` returns `None` when no number follows the cursor. It
  leaves the cursor where it is. The loop then repeats the same failed search
  forever.
- A digit string that is too large for an `isize` fails to parse. The loop then
  finds the same digits forever.

The user interface stops, and one thread uses all of its processor time. The
buffer size does not matter. A buffer of 12 characters is sufficient.

The fix stops the loop when no number remains, and moves the cursor past digits
that do not parse. Two tests in the same file cover both paths:

- `test_changenum_no_number_terminates`
- `test_changenum_unparsable_terminates`

Both tests do not finish against unmodified 0.0.25.

Upstream `main` still holds this defect. No patch was sent to upstream yet.
