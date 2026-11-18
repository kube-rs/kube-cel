[private]
default:
    @just --list

# Node image for the live-apiserver recipes (parity, probe): the Kubernetes
# version the `tests/common` expectations were measured against (v1.36.1, cel-go
# v0.26.0). Pinned by digest (the multi-arch index from the kind v0.32.0 release
# notes) so a re-pushed tag cannot change the ground truth. Override with
# KIND_NODE_IMAGE.
kind_node_image := env("KIND_NODE_IMAGE", "kindest/node:v1.36.1@sha256:3489c7674813ba5d8b1a9977baea8a6e553784dab7b84759d1014dbd78f7ebd5")

# --- CI / pre-publish checks (single source of truth) ---

# Run all checks — CI runs this, you should too before push
check: fmt clippy test-all test-no-default doc feature-check

# Format check (nightly required for latest rustfmt)
fmt:
    cargo +nightly fmt --check

# Clippy with all features, across all targets (lib, tests, examples, benches)
clippy:
    cargo clippy --all-features --all-targets -- -D warnings

# Test with all features (catches cross-feature issues)
test-all:
    cargo test --all-features

# Test with no default features
test-no-default:
    cargo test --no-default-features

# Check each feature compiles independently
feature-check:
    #!/usr/bin/env bash
    set -euo pipefail
    for feature in strings lists sets regex_funcs urls ip semver_funcs format quantity named_format validation; do
        echo "--- checking feature: $feature ---"
        cargo check --no-default-features --features "$feature"
    done

# Build docs (warnings = errors). Both feature sets: the all-features build is
# what hid the broken crate-root intra-doc links, so the default build is the
# guard that catches them. The final line reproduces the docs.rs build exactly
# (nightly + `--cfg docsrs`, all features, per [package.metadata.docs.rs]): a
# stable `cargo doc` never exercises the `#![cfg_attr(docsrs, feature(...))]`
# attrs, so docsrs-only breakage (e.g. a nightly feature removed upstream) slips
# through every stable doc build. This is the guard that catches it.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
    RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --all-features

# Live apiserver parity: spin up a throwaway kind cluster, run the gated parity
# tests (kube-cel verdict vs real apiserver `--dry-run=server`), tear down.
# Requires docker + kind + kubectl. Catches escaping/divergence regressions that
# a hand-written expectation would miss (see tests/apiserver_parity.rs).
parity:
    #!/usr/bin/env bash
    set -euo pipefail
    cluster=kubecel-parity
    trap 'kind delete cluster --name "$cluster" >/dev/null 2>&1 || true' EXIT
    kind create cluster --name "$cluster" --image "{{kind_node_image}}" --wait 90s
    export KUBE_CEL_PARITY_CTX="kind-$cluster"
    cargo test --features validation --test apiserver_parity -- --ignored --nocapture --test-threads=1

# Expression-level VALUE probe: harvest every CEL expression literal out of the
# unit tests, evaluate it with kube-cel, and ask a real apiserver whether
# `<expr> == <our value>` holds. Catches wrong *values* (not just wrong
# verdicts), including in the standard library inherited from the `cel` crate —
# the layer the hand-written unit-test expectations cannot vouch for.
# Reuses an existing cluster via KUBE_CEL_PARITY_CTX; otherwise makes its own.
probe:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "${KUBE_CEL_PARITY_CTX:-}" ]; then
        cargo test --features validation --lib validation::probe -- --ignored --nocapture --test-threads=1
        exit 0
    fi
    cluster=kubecel-probe
    trap 'kind delete cluster --name "$cluster" >/dev/null 2>&1 || true' EXIT
    kind create cluster --name "$cluster" --image "{{kind_node_image}}" --wait 90s
    export KUBE_CEL_PARITY_CTX="kind-$cluster"
    cargo test --features validation --lib validation::probe -- --ignored --nocapture --test-threads=1

# Phase-2 fidelity SWEEP: spin a kind cluster, measure the ACTUAL apiserver
# bucket for every candidate case under target/sweep/*.json (authored by the
# agent fan-out), write target/sweep/_results.json + print the matrix, tear down.
# Records the kind server version it ran against (the matrix is version-specific).
sweep:
    #!/usr/bin/env bash
    set -euo pipefail
    cluster=kubecel-sweep
    trap 'kind delete cluster --name "$cluster" >/dev/null 2>&1 || true' EXIT
    kind create cluster --name "$cluster" --wait 90s
    export KUBE_CEL_PARITY_CTX="kind-$cluster"
    kubectl --context "kind-$cluster" version -o json | grep -A6 serverVersion || true
    cargo test --features validation --test sweep_kind -- --ignored --nocapture --test-threads=1

# --- Development helpers ---

# Format fix
fmt-fix:
    cargo +nightly fmt

# Test default features only
test:
    cargo test

# Bump version and update all references (e.g., just bump 0.6.0)
bump version:
    #!/usr/bin/env bash
    set -euo pipefail
    old=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    old_minor="${old%.*}"  # e.g. 0.5.0 → 0.5
    new_minor="{{version}}"
    new_minor="${new_minor%.*}"  # e.g. 0.6.0 → 0.6
    sedi() { if [[ "$OSTYPE" == "darwin"* ]]; then sed -i '' "$@"; else sed -i "$@"; fi; }
    # Cargo.toml
    sedi 's/^version = ".*"/version = "{{version}}"/' Cargo.toml
    # README.md + src/lib.rs — update version in dependency examples
    sedi "s/kube-cel = \"${old_minor}\"/kube-cel = \"${new_minor}\"/g" README.md
    sedi "s/version = \"${old_minor}\"/version = \"${new_minor}\"/g" README.md src/lib.rs
    # Changelog: date an existing `## [X.Y.Z] - Unreleased` section (notes
    # written as the work landed), otherwise add an empty one.
    date=$(date +%Y-%m-%d)
    v_re=$(printf '%s' "{{version}}" | sed 's/\./\\./g')
    if grep -q "^## \[${v_re}\] - Unreleased$" CHANGELOG.md; then
        sedi "s/^## \[${v_re}\] - Unreleased$/## [{{version}}] - ${date}/" CHANGELOG.md
    elif grep -q "^## \[${v_re}\]" CHANGELOG.md; then
        echo "⚠ CHANGELOG.md already has a dated section for {{version}}; left it as it is"
    else
        entry="## [{{version}}] - ${date}\n\n### Added\n\n### Fixed\n\n### Changed\n"
        sedi "s/^# Changelog$/# Changelog\n\n${entry}/" CHANGELOG.md
    fi
    echo "Bumped ${old} → {{version}}"
    echo "Updated: Cargo.toml, README.md, src/lib.rs, CHANGELOG.md"
    echo "Edit CHANGELOG.md to fill in release notes, then commit with sign-off:"
    echo "  git commit -s -am 'chore: release {{version}}'   (or a descriptive message)"

# Run on the merged release commit (HEAD == origin/main, clean tree); never commits.
# Release: publish, then tag and push only that tag, then the GitHub release
release:
    #!/usr/bin/env bash
    # Land the version bump on main FIRST (`just bump X.Y.Z`, edit CHANGELOG,
    # `git commit -s`, merge). This recipe refuses a dirty tree and never creates
    # commits, so every release commit carries a DCO sign-off and a real message
    # (an old recipe made an unsigned `chore: release` commit, failing kube-rs DCO).
    set -euo pipefail
    version=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    tag="v${version}"
    v_re=$(printf '%s' "${version}" | sed 's/\./\\./g')
    if ! grep -Eq "^## \[${v_re}\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$" CHANGELOG.md; then
        echo "⚠ no dated CHANGELOG.md section for ${version} (\`just bump ${version}\` dates it)"
        exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
        echo "⚠ working tree is dirty — commit the version bump first (git commit -s)."
        echo "  This recipe never commits, so the release commit keeps your sign-off."
        git status --short
        exit 1
    fi
    git fetch --quiet origin main
    if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
        echo "⚠ HEAD is not origin/main — release the merged release commit:"
        echo "  HEAD        $(git log -1 --format='%h %s' HEAD)"
        echo "  origin/main $(git log -1 --format='%h %s' origin/main)"
        exit 1
    fi
    if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null \
        || [ -n "$(git ls-remote --tags origin "refs/tags/${tag}")" ]; then
        echo "⚠ tag ${tag} already exists (locally or on origin)"
        exit 1
    fi
    # The gate runs after the cheap refusals above, on the commit being released.
    just check
    echo "Releasing ${tag}..."
    # Publish before tagging: a failed publish leaves no tag anywhere. Only this
    # tag is pushed (never --tags, which would push every stray local tag).
    cargo publish
    git tag "${tag}"
    if ! git push origin "refs/tags/${tag}"; then
        echo "⚠ published ${version}, but pushing ${tag} failed; retry with:"
        echo "  git push origin refs/tags/${tag}"
        echo "  gh release create ${tag} --title ${tag} --notes-file <(just release-notes ${version}) --verify-tag"
        exit 1
    fi
    # GitHub release: notes reflowed to one line per paragraph/bullet (GitHub
    # renders single newlines as <br>, so hard-wrapped CHANGELOG text breaks
    # mid-sentence otherwise).
    gh release create "${tag}" --title "${tag}" \
        --notes-file <(just release-notes "${version}") --verify-tag
    echo "Published + released kube-cel ${tag}"

# Print GitHub-friendly (reflowed) release notes for VERSION from the CHANGELOG
release-notes version:
    #!/usr/bin/env python3
    # Each paragraph/bullet is reflowed onto a single line, because GitHub
    # renders single newlines as <br> (hard-wrapped CHANGELOG text would break
    # mid-sentence). Used by `release`; also handy for re-editing a release:
    #   gh release edit vX.Y.Z --notes-file <(just release-notes X.Y.Z)
    import re
    version = "{{version}}"
    lines = open('CHANGELOG.md').read().splitlines()
    start = next(i for i, l in enumerate(lines) if l.startswith(f'## [{version}]')) + 1
    sec = []
    for l in lines[start:]:
        if l.startswith('## ['):
            break
        sec.append(l)
    out, cur, fence = [], None, False
    def flush():
        global cur
        if cur:
            prefix, parts = cur
            out.append(prefix + ' '.join(p.strip() for p in parts))
            cur = None
    for l in sec:
        s = l.strip()
        # A fenced code block passes through verbatim (indentation kept, so it
        # stays inside its list item).
        if s.startswith('```'):
            flush(); out.append(l); fence = not fence; continue
        if fence:
            out.append(l); continue
        if s == '':
            flush()
            if out and out[-1] != '':
                out.append('')
            continue
        if re.match(r'^#{1,6}\s', s):
            flush(); out.append(s); continue
        m = re.match(r'^(\s*)([-*+]|\d+\.)\s+(.*)$', l)
        if m:
            flush(); cur = (f'{m.group(1)}{m.group(2)} ', [m.group(3)]); continue
        if cur:
            cur[1].append(s)
        else:
            # Keep the indentation of a paragraph that continues a list item.
            cur = (l[:len(l) - len(l.lstrip())], [s])
    flush()
    while out and out[-1] == '':
        out.pop()
    print('\n'.join(out))

# Dry-run publish
publish-dry: check
    cargo publish --dry-run
