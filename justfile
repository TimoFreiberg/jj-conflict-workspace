# jj-conflict-workspace release helpers.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Print the current Cargo package version.
version:
    @cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; pkgs=json.load(sys.stdin)["packages"]; assert len(pkgs) == 1, "expected exactly one package"; print(pkgs[0]["version"])'

# Commit the version bump before running this. The recipe refuses a dirty
# working copy, a HEAD without the release workflow, an already-existing
# tag, or a release commit that main cannot fast-forward to. Untracked
# files do not block the release.
# Advance `main` to the release commit and push `main` together with the
# `v<version>` tag, so the tag push finds the release workflow on the
# default branch and triggers it.
release:
    #!/usr/bin/env bash
    set -euo pipefail

    version="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; pkgs=json.load(sys.stdin)["packages"]; assert len(pkgs) == 1, "expected exactly one package"; print(pkgs[0]["version"])')"
    tag="v${version}"

    if [ -n "$(git status --porcelain | grep -v '^??')" ]; then
        echo "error: working copy has uncommitted changes to tracked files; commit the version bump first" >&2
        exit 1
    fi

    if [ ! -f .github/workflows/release.yml ]; then
        echo "error: .github/workflows/release.yml is not present at HEAD; commit the release workflow first" >&2
        exit 1
    fi

    if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
        echo "error: tag ${tag} already exists locally" >&2
        exit 1
    fi

    if git ls-remote --tags --exit-code origin "${tag}" >/dev/null 2>&1; then
        echo "error: tag ${tag} already exists on origin" >&2
        exit 1
    fi

    release_commit="$(git rev-parse HEAD)"
    if ! git merge-base --is-ancestor main "${release_commit}"; then
        echo "error: main is not an ancestor of the release commit; refusing to move main backwards" >&2
        exit 1
    fi

    echo "Creating tag ${tag} at $(git rev-parse --short HEAD)"
    git tag "${tag}"

    # Advance the main bookmark to the release commit so the tag push below
    # finds the release workflow on the default branch. No-op when main is
    # already at the release commit.
    if [ "$(git rev-parse refs/heads/main)" != "${release_commit}" ]; then
        echo "Advancing main to the release commit"
        git branch -f main "${release_commit}"
    fi

    echo "Pushing main and ${tag} to origin (tag push triggers the release workflow)"
    git push origin main "${tag}"

    remote_url="$(git remote get-url origin 2>/dev/null || true)"
    case "$remote_url" in
        *github.com*)
            repo_path="$(printf '%s' "$remote_url" | sed -E -e 's#^git@([^:]+):#\1/#' -e 's#^https?://##' -e 's#\.git$##')"
            echo "Watch the run: https://${repo_path}/actions"
            ;;
    esac
