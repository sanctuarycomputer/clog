# Working in this repo

This is a fork of BogKit (fold / ese / anny + examples). The main work here is
**clog**, an orientation engine library being built at `examples/clog/` against
the spec in `docs/clog-spec-v1.md` and the build design in
`docs/superpowers/specs/2026-08-15-clog-build-design.md`. Resolved fold
capability questions live in `docs/fold-answers.md`.

## Standing rules

- Any change to clog's public API or module map must update
  `examples/clog/README.md` and the relevant rustdoc **in the same change**.
- Spec invariants (INV-1..13 in `docs/clog-spec-v1.md`) are enforced by named
  tests; changing behavior a golden test freezes requires a spec edit first.
- All time comes from the Clock port; never read wall-clock outside it. No
  HashMap iteration order at any output boundary.
- Iterate with `--no-default-features` for fast compiles (ese's embedded map is
  slow to build); run full-feature tests before claiming a milestone done.
- Do not commit the garden3d observations corpus; only the human-reviewed,
  pseudonymized sample in `examples/clog/tests/fixtures/` may land in git.
