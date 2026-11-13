# Releasing

The CLI (`sqlx-sqruff`) and the library (`sqlx-sqruff-core`) share one version:
`version` under `[workspace.package]` in the root `Cargo.toml`. From 1.0 on, the
library follows semantic versioning for its public API. Every release publishes
both crates, so the version must be new for both.

1. Set `version` under `[workspace.package]` in the root `Cargo.toml`, and the
   same version on the `sqlx-sqruff-core` dependency under
   `[workspace.dependencies]` in that file. Merge to `main`.
2. Tag that commit `v<version>` (for example `v1.0.0`) and push the tag.

The `Release` workflow then runs the full CI and checks, before publishing
anything, that:

- the tagged commit is on `main`,
- the tag matches the version,
- neither crate's version is on crates.io yet, so a release that reuses a
  published version fails here instead of halfway through the publish.

It then publishes both crates to crates.io (library first) through trusted
publishing, creates the GitHub release and attaches the binaries.

If the `publish` job fails after the library was uploaded, for example on a
network error while uploading the CLI, open the workflow run and choose
**Re-run failed jobs**. That re-runs only the failed job and the jobs after
it. The `publish` job skips a crate whose version is already on crates.io, so
it resumes with the CLI. Do not choose **Re-run all jobs**: the version check
would then fail, because the library version is already published.
