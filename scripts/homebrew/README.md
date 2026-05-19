# Homebrew distribution

The Homebrew formulae live at the repo root under [`Formula/`](../../Formula).
This directory only exists for historical context — keep edits in
`Formula/dotagent.rb` (stable) and `Formula/dotagent@beta.rb` (beta).

## Two channels

| Formula                          | Tracks                | Release cadence                   |
|----------------------------------|-----------------------|-----------------------------------|
| `Formula/dotagent.rb`            | tagged releases (`v*.*.*`) | Manual — bump version, tag, push |
| `Formula/dotagent@beta.rb`       | `main` branch              | Automatic — every push to main   |

### Stable

```bash
brew tap avelino/dotagent https://github.com/avelino/dotagent
brew install dotagent
brew services start dotagent
```

### Beta (rolling, tracks `main`)

```bash
brew tap avelino/dotagent https://github.com/avelino/dotagent
brew install dotagent@beta
brew link --overwrite dotagent@beta
brew services start dotagent@beta
```

`dotagent@beta` is `keg_only` per Homebrew convention for versioned
formulae, so `brew link --overwrite` is needed to put `dotagent` on
`$PATH`. Switch back to stable with `brew unlink dotagent@beta &&
brew link dotagent`.

## Release flow

### Stable (`v*.*.*` tag)

1. Bump `version` in `Cargo.toml` (workspace package).
2. Update `CHANGELOG.md`.
3. Tag: `git tag -a v0.0.1 -m "release 0.0.1"` (push manually).
4. `.github/workflows/release.yml` runs on tag push:
   - **build** matrix produces `dotagent-<version>-<arch>-<os>.tar.gz`
     + `.sha256` for the 4 release targets.
   - **release** publishes the GitHub Release with both files.
   - **homebrew** rewrites the placeholders in
     `Formula/dotagent.rb` (anchored on `# DOTAGENT_*_SHA` markers)
     and commits + pushes to `main` as `github-actions[bot]`.

### Beta (every push to `main`)

`.github/workflows/release-beta.yml` runs on `push: branches: [main]`
with `paths-ignore` for `Formula/**`, `docs/**`, and `*.md` to avoid
rebuilding on doc-only or bot-driven changes:

1. **prepare** computes `BETA_VERSION = "<base>-beta.<git rev-list --count HEAD>"`.
2. **build** matrix produces `dotagent-beta-<arch>-<os>.tar.gz` + `.sha256`
   with version-less filenames (so the URL stays stable across pushes).
3. **release**:
   - `gh release delete beta --cleanup-tag` removes the previous rolling release.
   - `softprops/action-gh-release@v2` republishes the `beta` tag at the
     new commit with `prerelease: true` and `make_latest: false`.
4. **homebrew** rewrites the placeholders in `Formula/dotagent@beta.rb`
   (anchored on `# DOTAGENT_BETA_*_SHA` markers + `# DOTAGENT_BETA_VERSION`)
   and commits + pushes to `main`.

No manual sha256 copy required for either channel.

## Why no separate tap repo

A standalone `homebrew-dotagent` repo would let users run
`brew tap avelino/dotagent` without the URL. Trade-off: two repos to
keep in sync, one extra place where the formulae can drift. The
single-repo layout collapses both into one source of truth at the
cost of a one-time URL on the tap command.

## Loop protection (beta workflow)

The beta workflow commits back to `main` from `github-actions[bot]`,
which would normally retrigger the workflow. Two guards prevent that:

1. **`paths-ignore: ['Formula/**', ...]`** — the bot's commit only
   touches `Formula/dotagent@beta.rb`, so the trigger filter rejects it.
2. **`if: github.actor != 'github-actions[bot]'`** on the `prepare`
   job — even if the path filter misses, the actor check kills the
   run before any build starts.
