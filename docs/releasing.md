# Releasing andris

A release publishes andris to PyPI as two wheels, one for x86_64 and
one for aarch64, and attaches the same wheels to a GitHub release. It
takes two stages, so that the version bump goes through review like
any other change:

1. **Propose.** `make propose-release X.Y.Z` creates a
   `release-X.Y.Z` branch from `develop`, bumps the version, runs
   pre-commit and the tests, and pushes the branch. You open a pull
   request from it and land it as usual.
2. **Tag.** Once that pull request has landed, `make tag-release X.Y.Z`
   tags the merged commit `vX.Y.Z` and pushes the tag. The tag starts
   the
   [release workflow](https://github.com/shakenfist/andris/blob/develop/.github/workflows/release.yml),
   which builds, signs and publishes.

PyPI never accepts the same version twice, even after a release is
deleted, so the second stage cannot be taken back.

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

### Host tools

Both scripts run on the operator's host. Cargo runs only inside the
devcontainer, through `make lock` and `make test`, so the host needs
no Rust toolchain. They need:

- `docker`, for those two targets and for pre-commit's `make lint`;
- `git`, `make`, `curl` and `pre-commit`;
- `gh`, signed in (`gh auth login`), for `tag-release` to watch the
  workflow.

## Stage 1: propose the release

From a clean checkout of `develop`, in step with `origin`:

```bash
make propose-release 0.1.0
```

The script:

1. checks the version is `X.Y.Z`, and newer than the current one;
2. checks the checkout is on `develop`, clean and in step with
   `origin/develop`, and that neither `v0.1.0` nor `release-0.1.0`
   exists locally or on `origin`;
3. checks that PyPI does not already have `andris` 0.1.0;
4. creates and switches to `release-0.1.0`, and runs
   `pre-commit run --all-files`;
5. sets `version` in the root `Cargo.toml`'s `[workspace.package]`,
   which the crate inherits, and runs `make lock`, which moves the
   `andris` entry in `Cargo.lock` to match;
6. runs the release workflow's own version check against `v0.1.0`,
   and checks that only `Cargo.toml` and `Cargo.lock` changed;
7. runs `make test`;
8. shows the diff and asks `Commit and push release-0.1.0? [y/N]`;
9. on `y`, commits `Release 0.1.0.` and pushes the branch.

Nothing reaches `origin` before the prompt. On `N`, or if any step
fails, the script switches back to `develop` and deletes the branch.

Open a pull request from `release-0.1.0` into `develop`, and land it
through the merge queue like any other change. Its merge-tier
`Build the wheel` job builds and smoke-tests the x86_64 wheel at the
new version.

## Stage 2: tag the merged commit

After the pull request has landed:

```bash
make tag-release 0.1.0
```

The script:

1. fetches `origin/develop` and its tags;
2. checks that `v0.1.0` does not exist locally or on `origin`;
3. finds the commit that landed the release: the merge of the pull
   request holding the commit `Release 0.1.0.`, or that commit itself
   if it landed directly. It refuses if there is none, which is to say
   the release pull request has not landed;
4. checks that this commit's `Cargo.toml` and `Cargo.lock` both say
   0.1.0;
5. shows the commit it will tag, lists anything that landed on
   `develop` after it (which is not in the release), and asks
   `Create and push tag v0.1.0? [y/N]`;
6. on `y`, creates the annotated tag at that commit and pushes it;
7. finds the workflow run the tag started, watches it with
   `gh run watch`, and opens the release page when it finishes.

## What the workflow does

| Job | Runs on | What it does |
|-----|---------|--------------|
| `check-version` | static pool | The tag must name `[workspace.package]`'s version, and `Cargo.lock` must agree. |
| `build-wheels` | x86_64: the self-hosted `debian-13-docker` `l` pool; aarch64: GitHub's `ubuntu-24.04-arm` | Builds each wheel natively with `scripts/build-andris-wheel.sh`, the script `make wheel` runs, then smoke-tests it with `scripts/wheel-smoke.sh`. |
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
uploads the wheels as artifacts. `sign-tag`, `github-release` and
`publish-pypi` run only for a pushed tag, and show as skipped.

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
    --certificate-identity-regexp '^https://github.com/shakenfist/andris/' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

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
An administrator deletes the tag, and the version is fixed on
`develop` through a new release pull request:

```bash
git push origin :refs/tags/v0.1.0
git tag -d v0.1.0
```

### A wheel fails to build

Nothing is signed or published until both legs of `build-wheels`
pass. Fix the build on `develop` through a pull request (start the
workflow by hand on its branch to try both architectures), then have
an administrator delete the tag as above and run
`make tag-release X.Y.Z` again. The version is unchanged, since
nothing reached PyPI.

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
