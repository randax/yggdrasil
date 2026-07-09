# kvasir Development Guidelines

- use jj when .jj exists, git otherwise
- Before implementation work begins, create a branch and open a draft PR whose
  title summarizes the intended work and whose description contains the plan.
  Make this the first action for all non-trivial tasks unless explicitly told
  not to.
- When creating or editing PR descriptions with `gh`, pass multiline Markdown
  with real newlines, never escaped `\n` sequences, and verify the rendered body
  with `gh pr view` before moving on.
- Divide work up, use sub-agents and role specific agents to achieve our task.
- After you are finished, use multiple adverserial personas subagents to review our changes; repeat until they find no real and actionable issues.
- Always update the PR title and desc to desc the complete work done, with the plan; and remove draft status
- Our job doesn't stop after we push, we always monitor CI and fix it until all checks are green

## Active Technologies
- Core buisniss logic component should be written in Rust and exposed using Uniffi.
- Minmal buiniss logic in SwiftUI layer. All if that logic should be in UniFFi so it is easier to add support for other platforms like linux and windows.

## Project Structure

```text
src/
tests/
```

## Commands

- Clippy hard rules:
  - Local and CI clippy checks MUST run with `-D warnings`; warnings are
    errors.
  - Do not add `#[allow(...)]`, `#![allow(...)]`, or clippy-specific
    suppressions unless absolutely necessary and explicitly justified in the
    PR description.
  - Prefer deleting dead code, wiring unused code into the exercised path, or
    narrowing visibility over suppressing warnings.

## Code Style

- Write single-purpose functions with explicit inputs and outputs; keep them
  small enough to review quickly and avoid multi-purpose helpers.
- Prefer many small, focused files over large modules; organize files around
  behavior, protocol boundary, or UI responsibility.
- Prefer functional patterns where they improve clarity: pure helpers,
  immutable data, composition, and local reasoning over hidden side effects.
- Avoid broad utility modules and implicit shared state. When functional style
  conflicts with local Rust, Swift, or framework idioms, choose the
  clearest conformant implementation.

## Commit Messages

- Use Conventional Commits with a single scope: `fix(chat): ...`, `fix(server): ...`, `feat(apple/ui): ...`.
- Do not use unscoped subjects. If a change spans multiple areas, scope it to the dominant subsystem/user-facing surface and keep the subject lowercase after the colon.

## Recent Changes


- Typed-payloads hard rule:
  - Protocol data MUST be modelled with typed Rust values — never `String`, `&str`, or `Vec<u8>` blobs — at every boundary: event enums, handler traits, actor messages, dispatcher entries, callback envelopes, storage writes, routing effects, and public return types.
  - Serialization to `String` / `Vec<u8>` happens only at the I/O boundary (the transport adapter writing bytes to a socket). Any event that is not the literal write-to-wire effect carries typed values.
  - Parsing untyped input (raw frames from the transport, rows from storage) into typed values happens exactly once, as early as possible, and the untyped form is dropped immediately — typed values flow through the rest of the system.
  - Error results are typed (`thiserror` enums or typed stanza-error structs), never stringly-typed diagnostics masquerading as payloads. `String` is acceptable only for human-facing log messages emitted via the `Log` outbound event.
  - A PR that introduces a new `String`/`&str` field on an event, message, trait method, or public struct to carry structured data MUST be rejected; convert to a typed value or add a typed enum variant instead.


  - If a feature is advertised but lacks testable behavior, either implement behavior with tests or remove the advertisement.

- Breaking changes by default: do not add backwards compatibility layers, migration shims, or legacy aliases unless explicitly requested.
- Assume no production servers/users/data for this project; prioritize clean design over compatibility.
- Keep the codebase clean: remove dead compatibility code immediately instead of preserving legacy paths.