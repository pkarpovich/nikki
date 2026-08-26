# nikki daemon conventions

`README.md` carries the wire contract, the permission model and the provider model; this file carries
only the rules that govern how code is written here. The plan that built the daemon, with the
verified API behaviour behind every decision, is `docs/plans/completed/2026-08-25-nikki-daemon.md`.

## Everything runs natively on macOS

The crate links Apple frameworks, so it does not build on any other target and cross-compiling needs
the Apple SDK. A gate that cannot link is a gate that proves nothing.

## Unsafe containment

Every `unsafe` block lives in `src/macos/` and nowhere else, including the CGWindowList and
CGDisplay wrappers. The rest of the daemon never sees a raw pointer, and the module's public API
exposes only safe types.

Core Foundation memory discipline is the live bug class: a value from a `Copy` or `Create` function
is owned and must be released on **every** path including error returns; a value from a `Get`
function is not. An Accessibility attribute is not guaranteed to hold the type its name suggests -
check the type id before wrapping, and release before returning the mismatch.

## Tests live inline

Tests go in a `#[cfg(test)] mod tests` block in the file they cover. A sibling `foo_test.rs` is not
compiled unless something declares it, so it would sit unbuilt while the gate reported success.

## Declare every module

Every new module file is declared the moment it is created - `mod x;` in its parent `mod.rs` or in
`main.rs`. An undeclared module is not compiled, and neither are its inline tests.

No module carries a blanket `#[allow(dead_code)]`: it is what the compiler uses to report a provider,
extractor or helper that was written and never wired up. An item that exists only for tests is
`#[cfg(test)]`, and a field held for ownership rather than reading carries its own narrow allow.

## Per-task gate

```
mise run check
! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'
```

`mise run check` is `cargo fmt --all -- --check`, then `cargo clippy --all-targets -- -D warnings`,
then `cargo test`. Both commands must pass before the next task starts.

A change to `Info.plist.template` or `scripts/bundle.sh` additionally requires
`./scripts/acceptance.sh`, which is the only thing that asserts `LSUIElement` and a non-empty
`NSAppleEventsUsageDescription` on the assembled bundle.

## Style

- No comments; clear names instead.
- Early return: handle the failure case first, keep the main path flat.
- A provider must never panic the process. A failing provider restarts alone.
- Every subprocess call has a deadline and is killed when it expires.
