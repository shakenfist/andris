# Releasing andris

A release publishes andris to PyPI as two wheels, one for x86_64 and
one for aarch64, and attaches the same wheels to a GitHub release. It
takes two stages, so that the version bump goes through review like
any other change:

1. **Propose.** `make propose-release X.Y.Z` creates a
   `release-X.Y.Z` branch from `develop`, bumps the version, runs
   pre-commit and the tests, pushes the branch and opens a pull
   request. You land it through the merge queue as usual.
2. **Tag.** Once that pull request has merged, `make tag-release X.Y.Z`
   tags the commit the queue merged `vX.Y.Z` and pushes the tag. The
   tag starts the
   [release workflow](https://github.com/shakenfist/andris/blob/develop/.github/workflows/release.yml),
   which checks that CI passed on that commit, then builds, signs and
   publishes.

PyPI never accepts the same version twice, even after a release is
deleted, so the second stage cannot be taken back.

The two scripts, `tools/propose-release.sh` and `tools/tag-release.sh`,
are the Shaken Fist Rust release template's, as are
`tools/release-version.sh` and `tools/sign-release-tag.sh`, and the
workflow's jobs other than `build-wheels` and `publish-pypi`. The
template, and the invariants a release keeps, are in
[shakenfist/development](https://github.com/shakenfist/development/tree/main/templates/release-automation-rust).
Change them there first, then copy them here; only the marked
per-repository settings at the top of `propose-release.sh` and
`release-version.sh` are andris's own.

## Prerequisites

These are set up once, by a repository administrator, before the
first release. The workflow cannot publish without all three.

### The PyPI trusted publisher

The wheels are published through a PyPI *trusted publisher*: PyPI
accepts an upload from this repository's workflow on the strength of
GitHub's OIDC token, and no PyPI token is stored anywhere.

Before the first release the `andris` project does not exist on
PyPI, so the publisher is added as a *pending* publisher. On
[pypi.org](https://pypi.org), go to **Your account → Publishing →
Add a new pending publisher → GitHub**, and enter:

| Field | Value |
|-------|-------|
| PyPI project name | `andris` |
| Owner | `shakenfist` |
| Repository name | `andris` |
| Workflow name | `release.yml` |
| Environment name | `release` |

The first successful publish creates the project and turns the
pending publisher into an ordinary one, found afterwards under the
project's **Settings → Publishing**. A pending publisher does not
reserve the name: if someone else registers `andris` first, it
lapses.

PyPI matches all five values exactly. Renaming the workflow file or
the environment breaks publishing until the publisher is changed to
match.

### The `release` environment

Under the repository's **Settings → Environments**, create an
environment named `release`, and set:

- **Required reviewers:** the maintainers who may approve a release.
  Leave **Prevent self-review** off if the person who pushes the tag
  is also the one who approves it.
- **Deployment branches and tags:** **Selected branches and tags**,
  with one rule, the tag pattern `v*`. The workflow already confines
  its publishing jobs to a pushed tag; this makes the environment
  refuse anything else as well.

The environment gates the `sign-tag` job, and every publishing job
needs `sign-tag`, so nothing is signed or published until a reviewer
approves the run. `publish-pypi` names the environment too, because
PyPI's trusted publisher matches on it, and it becomes ready after
`sign-tag` has finished, so **a release asks for approval twice**.

If the environment is missing, GitHub creates it on the first run
with no reviewers, and the release then runs without waiting for
anyone. Create it before the first tag.

### Tag protection

Under **Settings → Rules → Rulesets**, add a tag ruleset:

- **Name:** `Restrict releases`
- **Enforcement:** Active
- **Target tags:** include the pattern `v*`
- **Rules:** **Restrict creations**, **Restrict deletions** and
  **Block force pushes**
- **Bypass list:** the **Repository admin** role, always

This stops anyone but an administrator from creating, moving or
deleting a release tag. Ryll's release tags are protected the same
way.

### The default branch

The repository's default branch on GitHub must be `develop`, as it
is today. The scripts do not name the branch: they ask `gh repo view`
for the default branch, so `propose-release` branches from it and
`tag-release` looks for the release commit on its first-parent
history. The merge queue, its ruleset and the CI gate all work on
`develop`, so if the default branch were changed to `main`, both
scripts would follow it and stop matching what the queue merged.

### Host tools

Both scripts run on the operator's host. Cargo runs only inside the
devcontainer, through `make lock` and `make test`, so the host needs
no Rust toolchain. They need:

- `docker`, for those two targets and for pre-commit's `make lint`;
- `git`, `make`, `curl` and `pre-commit`;
- `gh`, signed in (`gh auth login`), to find the default branch, for
  `propose-release` to open the pull request, and for `tag-release` to
  watch the workflow.

## Stage 1: propose the release

From a clean checkout of `develop`, in step with `origin`:

```bash
make propose-release 0.1.0
```

The script:

1. checks the version is `X.Y.Z`;
2. checks the checkout is on `develop`, clean and in step with
   `origin/develop`, and that neither `v0.1.0` nor `release-0.1.0`
   exists locally or on `origin`;
3. checks the version is newer than the current one, and that PyPI
   does not already have `andris` 0.1.0;
4. creates and switches to `release-0.1.0`;
5. sets `version` in the root `Cargo.toml`'s `[workspace.package]`,
   which the crate inherits, and runs `make lock`, which moves the
   `andris` entry in `Cargo.lock` to match, and refuses to go on if
   that left an untracked file;
6. runs `pre-commit run --all-files` over the bumped tree, then the
   release workflow's own version check against `v0.1.0`;
7. runs `make test`;
8. shows the diff and asks
   `Commit, push release-0.1.0 and open a pull request? [y/N]`;
9. on `y`, commits `Release 0.1.0.`, pushes the branch and opens a
   pull request into `develop`.

Nothing reaches `origin` before the prompt. On `N`, or if any step
fails before the push, the script switches back to `develop` and
deletes the branch.

Get the pull request reviewed, and land it through the merge queue
like any other change. Its merge-tier `Build the wheel` job builds and
smoke-tests the x86_64 wheel at the new version, and the queue's run
on the merged commit is the CI result the release checks.

## Stage 2: tag the merged commit

After the pull request has merged through the queue:

```bash
make tag-release 0.1.0
```

The script:

1. fetches `origin/develop` and its tags;
2. checks that `v0.1.0` does not exist locally or on `origin`;
3. finds the commit that merged the release: the first commit on
   `develop`'s first-parent history that contains the commit
   `Release 0.1.0.` (or `Release 0.1.0. (#N)`, after a squash merge),
   which is the queue's merge rather than `develop`'s tip. It refuses
   if there is none, which is to say the release pull request has not
   merged;
4. checks that this commit is on `develop`'s first-parent history,
   where the release's CI gate looks, and that its `Cargo.toml` and
   `Cargo.lock` both say 0.1.0;
5. shows the commit it will tag, lists anything that merged into
   `develop` after it (which is not in the release), and asks
   `Create and push tag v0.1.0? [y/N]`;
6. on `y`, creates the annotated tag at that commit and pushes it,
   deleting the local tag again if the push fails;
7. finds the workflow run the tag started, watches it with
   `gh run watch`, and opens the release page when it finishes.

`tools/tag-release.sh 0.1.0 COMMIT` tags a later commit instead, for
recovering from a gate failure (see below).

## What the workflow does

| Job | Runs on | What it does |
|-----|---------|--------------|
| `ci-gate` | static pool | Fails the release unless `ci.yml` succeeded on the tagged commit. See [The CI gate](#the-ci-gate). |
| `check-version` | static pool | The tag must name `[workspace.package]`'s version, and `Cargo.lock` must agree. |
| `build-wheels` | x86_64: the self-hosted `debian-13-docker` `l` pool; aarch64: GitHub's `ubuntu-24.04-arm` | Builds each wheel natively with `scripts/build-andris-wheel.sh`, the script `make wheel` runs, checks its contents with `shakenfist/actions/check-dist`, then smoke-tests it with `scripts/wheel-smoke.sh`. |
| `sign-tag` | an ephemeral GitHub-hosted runner | After the first approval, replaces the tag with one signed keylessly through Sigstore's gitsign, and force-pushes it. |
| `github-release` | static pool | Checks that exactly the two wheels arrived, writes `SHA256SUMS`, and creates the GitHub release with generated notes. |
| `publish-pypi` | the self-hosted `debian-13-docker` `s` pool, whose VMs are made for one job and destroyed | After the second approval, attests the wheels' build provenance and uploads them to PyPI. |

The wheel builds do not restore the cargo cache that pull request
runs save, so a release is built only from what `Cargo.lock` names,
downloaded afresh.

`publish-pypi` passes `skip-existing`, so a re-run after a partial
upload sends only what PyPI does not have yet.

### Trying the build before a tag

The merge tier builds only the x86_64 wheel. To build both before
tagging, after a change to the wheel build or a toolchain bump, start
the workflow by hand: **Actions → Release → Run workflow**, on any
branch. It runs `check-version` (which, with no tag, checks only that
`Cargo.toml` and `Cargo.lock` agree) and both `build-wheels` legs, and
uploads the wheels as artifacts. `ci-gate`, `sign-tag`,
`github-release` and `publish-pypi` run only for a pushed tag, and show
as skipped.

### The CI gate

`ci-gate` runs first, and every other job needs it, so nothing is
built, signed or published from a commit CI did not pass. It uses
[`shakenfist/actions/require-ci-success`](https://github.com/shakenfist/actions/blob/main/docs/actions.md#require-ci-success),
which requires a successful `ci.yml` run on the tagged commit. With
the merge queue, that is the merge tier's run on the commit the queue
merged, which is the commit `tag-release` picks. A commit with no such
run, because it was merged around the queue, or is not on `develop`'s
first-parent history, cannot be released. The gate has no override.

The Rust release template's README in
[shakenfist/development](https://github.com/shakenfist/development/tree/main/templates/release-automation-rust)
says what counts as CI, and links the recovery steps for each reason
the gate fails. The common cases are under Troubleshooting below.

## The artifacts

| Where | Artifact |
|-------|----------|
| PyPI and the GitHub release | `andris-X.Y.Z-py3-none-manylinux_2_28_x86_64.whl` |
| PyPI and the GitHub release | `andris-X.Y.Z-py3-none-manylinux_2_28_aarch64.whl` |
| The GitHub release | `SHA256SUMS`, for both wheels |
| The repository | the tag `vX.Y.Z`, signed with Sigstore |

Each wheel holds the compiled `andris` binary, which pip installs
onto `PATH`, with openh264 built from source into it. There is no
source distribution: the binary is the product, and building andris
from source needs ryll's crates from git, which belongs in a checkout
(see [Development](development.md)).

Check a downloaded wheel against the release, its provenance, and the
tag's signature, with:

```bash
sha256sum -c SHA256SUMS
gh attestation verify andris-0.1.0-py3-none-manylinux_2_28_x86_64.whl \
    --repo shakenfist/andris
gitsign verify-tag v0.1.0 \
    --certificate-identity-regexp '^https://github\.com/shakenfist/andris/\.github/workflows/release\.yml@refs/tags/v' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The pattern names `release.yml` running for a `v*` tag: a pattern that
stops at the repository would accept a signature from any of its
workflows. [gitsign](https://github.com/sigstore/gitsign) has
install instructions.

## The platform tag

The `manylinux_2_28` tag is earned, not asserted. The wheel is built
in the `manylinux_2_28` image, so it needs nothing newer than glibc
2.28, and the binary links only glibc and the C++ runtime, both of
which the policy allows. libpulse, for sound, is loaded when it is
first needed rather than linked, and andris serves without sound
where it is missing. The build therefore runs maturin's own policy
check, without `--skip-auditwheel`, and fails if the binary ever links
a library outside the policy. There is no list of exceptions to keep.

The build also fails if openh264's SIMD code is missing from the
binary, which happens silently when nasm is absent and makes encoding
far slower, and if the binary has an RPATH or RUNPATH, which would be
searched for libpulse ahead of the host's own library directories.

Andris's end-to-end tests run only on x86_64. The aarch64 wheel is
checked by the same build checks and the same smoke test, which runs
it in a container, but never against a real desktop.

## Troubleshooting

### Version mismatch

`check-version` fails when the tag does not name the workspace's
version, or `Cargo.lock` disagrees with `Cargo.toml`. That should not
happen through `tag-release`, which checks both first. If it does,
after a tag pushed by hand, say, nothing has been signed or published.
The version is fixed on `develop` through a new pull request, and then an
administrator moves the tag to the fixed commit. Name that commit:
a fix pull request is not a "Release X.Y.Z." commit, so without it
`tag-release` would pick the original release merge and refuse:

```bash
git push origin :refs/tags/v0.1.0   # needs the tag ruleset's bypass
git tag -d v0.1.0
tools/tag-release.sh 0.1.0 <the fixed commit>
```

### The CI gate fails

Nothing has been built, signed or published, since every other job
needs `ci-gate`. Its log names the reason:

- **CI is still running, or failed because of a flake** on the tagged
  commit: wait for it, or re-run the failed `ci.yml` jobs, then re-run
  the failed release jobs.
- **The tagged commit has no `ci.yml` run**, because it was merged
  around the merge queue, or is not on `develop`'s first-parent
  history: move the tag to a later commit that went through the queue
  and carries the same version:

  ```bash
  git push origin :refs/tags/v0.1.0   # needs the tag ruleset's bypass
  git tag -d v0.1.0
  tools/tag-release.sh 0.1.0 <commit that went through the queue>
  ```

### A wheel fails to build

Nothing is signed or published until both legs of `build-wheels`
pass. That includes `check-dist`, which fails a wheel holding a test
directory, or nothing but its metadata. Fix the build on `develop`
through a pull request (start the workflow by hand on its branch to
try both architectures), then have an administrator move the tag to
the fixed commit as above, with
`tools/tag-release.sh X.Y.Z <the fixed commit>`. The version is
unchanged, since nothing reached PyPI.

### The release waits for approval

`sign-tag`, and later `publish-pypi`, wait for a reviewer to approve
the `release` environment. When `gh run watch` stops moving after the
builds, or again after `github-release`, approve the run in the
Actions page.

### `sign-tag` cannot push the tag

The tag ruleset blocks force pushes for everyone off its bypass list,
and `sign-tag` force-pushes the signed tag with the workflow's token.
Ryll's identical ruleset lets that through. If GitHub refuses it here,
the push error names the ruleset. Nothing has been published, since
the publishing jobs need `sign-tag`. Add the **GitHub Actions** app to
the ruleset's bypass list, and re-run the failed jobs.

### PyPI refuses the upload

`publish-pypi` uses no stored credential, so its failures are almost
always configuration:

- **"Publisher not found"** or **invalid-publisher**: the trusted
  publisher does not match the owner, repository, workflow file or
  environment exactly. Compare it with the table above.
- **A partial upload**, one wheel published and the other not: re-run
  the failed job. `skip-existing` sends only the missing wheel.

A failure here leaves the GitHub release in place, since it runs
first. Re-run `publish-pypi` once the cause is fixed; do not publish
the version some other way.
