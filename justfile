# jj-conflict-workspace release helpers.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Print the current Cargo package version.
version:
    @cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; pkgs=json.load(sys.stdin)["packages"]; assert len(pkgs) == 1, "expected exactly one package"; print(pkgs[0]["version"])'

# Bump the version to <version>, commit the bump, tag it `v<version>` and
# push. Without an argument, release the version already in Cargo.toml at
# HEAD (commit any bump yourself first). The recipe refuses a dirty
# working copy, a HEAD without the release workflow, an already-existing
# tag, or a release commit that main cannot fast-forward to. Untracked
# files do not block the release.
# Advance `main` to the release commit and push `main` together with the
# `v<version>` tag, so the tag push finds the release workflow on the
# default branch and triggers it.
release version='':
    #!/usr/bin/env bash
    set -euo pipefail

    version={{quote(version)}}

    read_package_version() {
        cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; pkgs=json.load(sys.stdin)["packages"]; assert len(pkgs) == 1, "expected exactly one package"; print(pkgs[0]["version"])'
    }

    if [ -n "${version}" ]; then
        case "${version}" in
            v*) version="${version#v}" ;;
        esac
        if ! printf '%s' "${version}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-.+][0-9A-Za-z.-]+)?$'; then
            echo "error: invalid version '${version}'; expected a version like 0.2.0" >&2
            exit 1
        fi
    fi

    current="$(read_package_version)"
    if [ -n "${version}" ] && [ "${version}" = "${current}" ]; then
        echo "error: version ${version} is already the current version; pass a new version or no argument to release the current one" >&2
        exit 1
    fi
    tag="v${version:-${current}}"

    if [ -n "$(git status --porcelain | grep -v '^??')" ]; then
        echo "error: working copy has uncommitted changes to tracked files; commit or discard them first" >&2
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

    if ! git merge-base --is-ancestor main "$(git rev-parse HEAD)"; then
        echo "error: main is not an ancestor of the release commit; refusing to move main backwards" >&2
        exit 1
    fi

    if [ -n "${version}" ]; then
        echo "Bumping version ${current} -> ${version} in Cargo.toml and Cargo.lock"
        # The heredoc body sits at the recipe base indentation (4 spaces) so
        # that `just` strips it to column 0, where bash expects the
        # terminator.
        python3 - Cargo.toml Cargo.lock "${version}" <<'PY'
    import re, sys

    toml_path, lock_path, version = sys.argv[1], sys.argv[2], sys.argv[3]

    with open(toml_path) as f:
        toml_lines = f.readlines()
    pkg_name, in_package, found_toml = None, False, False
    out = []
    for line in toml_lines:
        if line.lstrip().startswith('['):
            in_package = line.strip() == '[package]'
        elif in_package and re.match(r'^version\s*=', line):
            line = re.sub(r'^(version\s*=\s*)"[^"]*"', r'\1"%s"' % version, line)
            found_toml = True
        elif in_package and pkg_name is None and re.match(r'^name\s*=\s*', line):
            pkg_name = re.match(r'^name\s*=\s*"([^"]+)"', line).group(1)
        out.append(line)
    if not pkg_name or not found_toml:
        sys.exit('error: could not find the package name or version in Cargo.toml')
    with open(toml_path, 'w') as f:
        f.writelines(out)

    with open(lock_path) as f:
        lock_lines = f.readlines()
    out, found_lock, active, block_name = [], False, False, None
    for line in lock_lines:
        stripped = line.strip()
        if stripped.startswith('[['):
            active, block_name = True, None
        elif stripped.startswith('[') or stripped == '':
            active = False
        if active:
            m = re.match(r'^name\s*=\s*"([^"]+)"', stripped)
            if m:
                block_name = m.group(1)
            elif block_name == pkg_name and re.match(r'^version\s*=', stripped):
                line = re.sub(r'^(version\s*=\s*)"[^"]*"', r'\1"%s"' % version, line)
                found_lock = True
        out.append(line)
    if not found_lock:
        sys.exit('error: could not find the version of %s in Cargo.lock' % pkg_name)
    with open(lock_path, 'w') as f:
        f.writelines(out)
    PY

        bumped="$(read_package_version)"
        if [ "${bumped}" != "${version}" ]; then
            echo "error: Cargo.toml still reports version ${bumped}; expected ${version}" >&2
            exit 1
        fi

        echo "Committing version bump"
        jj commit Cargo.toml Cargo.lock -m "Bump version to ${version}" >/dev/null
        if [ -n "$(git status --porcelain | grep -v '^??')" ]; then
            echo "error: the version bump left uncommitted changes to tracked files; commit them and rerun" >&2
            exit 1
        fi
    fi

    release_commit="$(git rev-parse HEAD)"
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
