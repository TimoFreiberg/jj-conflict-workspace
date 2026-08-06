# jj-conflict-workspace release helpers.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Print the current Cargo package version.
version:
    @cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; pkgs=json.load(sys.stdin)["packages"]; assert len(pkgs) == 1, "expected exactly one package"; print(pkgs[0]["version"])'

# Commit the version bump before running this. The recipe refuses a dirty
# working copy, a HEAD without the release workflow, or an already-existing
# tag. Untracked files do not block the release.
# Create and push the `v<version>` tag and trigger the release workflow.
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
        echo "error: tag ${tag} already exists" >&2
        exit 1
    fi

    echo "Creating tag ${tag} at $(git rev-parse --short HEAD)"
    git tag "${tag}"

    echo "Pushing ${tag} to origin (triggers the release workflow)"
    git push origin "${tag}"

    remote_url="$(git remote get-url origin 2>/dev/null || true)"
    case "$remote_url" in
        *github.com*)
            repo_path="$(printf '%s' "$remote_url" | sed -E -e 's#^git@([^:]+):#\1/#' -e 's#^https?://##' -e 's#\.git$##')"
            echo "Watch the run: https://${repo_path}/actions"
            ;;
    esac
