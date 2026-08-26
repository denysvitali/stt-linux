# Release process

This project ships two GitHub Actions workflows for releases:

- **`binaries.yml`** — runs on every push to `master` and uploads the
  latest `stt` and `sttd` binaries as workflow artifacts (30-day
  retention). These track `master` and are the right thing to grab for
  bug reports, manual QA, or CI consumers that want a "last good"
  build without waiting for a release.
- **`release.yml`** — produces a real GitHub Release with binaries
  and an auto-generated changelog. Two ways in:
  - **Push a `vX.Y.Z` tag.** The workflow builds, runs the test
    suite, generates the changelog up to that tag with
    [git-cliff](https://git-cliff.org/), attaches the binaries, and
    publishes the release in one shot. This is the path for normal
    releases.
  - **Trigger from the Actions tab.** The workflow derives the next
    semver from conventional-commits history with
    `git cliff --bumped-version`, builds, runs tests, attaches the
    binaries to a *draft* release, and stops. Review the draft, then
    push the same tag yourself to publish — or pass
    `bump_and_release=true` to skip the review and ship in one shot.

## Tag convention

`vMAJOR.MINOR.PATCH`, e.g. `v0.1.0`. `cliff.toml` enforces this with
`tag_pattern = "v[0-9]+\\.[0-9]+\\.[0-9]+"`; typo tags like `v1` or
`v0_1_0` will not match the release trigger and the changelog will
skip them.

## Targets

`ubuntu-24.04`, glibc, x86_64. Cross-compile (macOS, Windows,
musl) is deliberately out of scope for `v0.1.x`: the `ort`/Parakeet
toolchain is fragile to cross and there is no concrete user demand
for it yet. Add a matrix when there is.

## Out of band

- The version in `Cargo.toml` is not auto-bumped. The tag is the
  source of truth; `cargo build` does not need it. If we ever want
  `stt --version` to print the release, wire it up to
  `git describe` or stamp it from CI.
- Releases are not signed (`cosign`/Sigstore). The model is ~610 MB
  unsigned, so signing the binaries without signing the model is
  mostly theatre. Revisit if we ever ship the model in-tree.
