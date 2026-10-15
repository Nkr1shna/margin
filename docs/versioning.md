# Versioning

Margin uses semantic versions (`MAJOR.MINOR.PATCH`) on every merge into `dev`, not only at
prod releases. Every build says which version it is: the service at `GET /api/version`
and in `margin-service version`, the app in Settings > General > About, the DMG's file
name, and the release title.

Margin stays below 1.0 until the owner says otherwise. Below 1.0 a breaking change bumps
the minor, as Cargo does: `0.4.2` with a breaking change becomes `0.5.0`.

## A PR declares its change

A PR into `dev` that changes code (`crates/`, `Cargo.toml`, `Cargo.lock`,
`rust-toolchain.toml`, `deploy/`, `.github/`) adds one changeset file,
`changes/<slug>.md`, with two lines:

```
level: minor
note: Settings shows the app's and the service's version.
```

Pick the level by what the person using Margin notices.

- `major`: the app and the service no longer work together, or a migration can't be
  undone. Below 1.0 this bumps the minor.
- `minor`: a new feature or a change in behaviour someone can see.
- `patch`: a fix, or internal, docs or CI work.

The note is one line for the changelog. Name the slug after the change, so two PRs never
write the same file. A PR that changes only docs needs no changeset.

CI's `Version` job (`ci.yml`) fails a code PR without a valid changeset. Dependabot PRs
may go without one; they keep the version they merge into. Nobody edits a version number
in a PR, so PRs never conflict over one.

## The version is computed

`scripts/version.sh at REV` computes a commit's version from git history.

1. It starts at the nearest `dev-v<version>` tag on dev's first-parent line. Without one,
   it starts at Cargo.toml's `[workspace.package]` version (`0.1.0`).
2. Each later commit that added changesets bumps once, by the highest level among them.
3. A commit without a changeset keeps its parent's version.
4. A merge commit (a release) has the version of the dev commit it merged.

Nothing commits a version to `dev`, which only takes squash-merged PRs. Cargo.toml stays at
the starting version. CI builds pass the computed version to the compiler as
`MARGIN_VERSION`; a local build reports Cargo.toml's.

- `tag-version.yml` tags each dev merge that bumped `dev-v<version>`, with its notes in
  the tag message. The tags only save walking history; the answer is the same without them.
- `build-service.yml` compiles `MARGIN_VERSION` into the service and checks the binary
  says it. `deploy.sh` prints the version it replaced and the one it started; `smoke-dev`
  checks `/api/version` reports the deployed commit's version.
- `apps.yml` compiles it into the app, stamps the bundle's `CFBundleShortVersionString`
  and `CFBundleVersion` with it, names the files `Margin-<environment>-<version>-<sha>`,
  and puts it in the release title. A later commit never has a lower version, so the
  bundle version only increases.
- `scripts/version.sh changelog` prints every version with its notes, newest first. Don't
  write a CHANGELOG.md by hand.

Plain `v<version>` tags are for release commits. `apps.yml` builds both environments on a
`v*` tag; `dev-v*` tags don't match it.

## Compatibility is a number, not the version

`GET /api/version` also reports three integers, which code compares instead of semver.

- `api_version` is the service's `API_VERSION` (`crates/shared`), the contract it serves.
- `min_client_api` is the oldest app contract it still serves (`MIN_CLIENT_API` in the
  service's `version.rs`). When it is above the app's `API_VERSION`, Settings > General >
  About tells the person to install the newest build.
- `schema_version` is the stored data's shape this build writes (`db::SCHEMA_VERSION`).

Raise `API_VERSION` when the service starts answering in a way an older app can't use, and
`MIN_CLIENT_API` with it once those apps should stop working. Such a change is `major`.

## Changing the scheme

The release model may change (a study of Zed's channels and release branches is open).
The parts are separate so one can change without the others.

- The rule a PR follows: `scripts/version.sh check` and CI's `Version` job.
- How a commit's version is found: `scripts/version.sh at`. A model that commits versions
  to Cargo.toml would make it read Cargo.toml.
- Where builds show it: `MARGIN_VERSION` in `build-service.yml` and `apps.yml`, the app's
  `build.rs`, and the service's `version::VERSION`.
- Tags: `tag-version.yml`.

`sh scripts/version-test.sh` tests `version.sh` offline against a throwaway repository.
