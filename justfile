[private]
default:
    @just --list

# --- CI / pre-publish checks (single source of truth) ---

# Run all checks — CI runs this, you should too before push
check: fmt clippy test-all test-no-default doc feature-check

# Format check (nightly required for latest rustfmt)
fmt:
    cargo +nightly fmt --check

# Clippy with all features
clippy:
    cargo clippy --all-features -- -D warnings

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
    for feature in strings lists sets regex_funcs urls ip semver_funcs format quantity jsonpatch named_format math encoders validation; do
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
    kind create cluster --name "$cluster" --wait 90s
    export KUBE_CEL_PARITY_CTX="kind-$cluster"
    cargo test --features validation --test apiserver_parity -- --ignored --nocapture --test-threads=1

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
    # Add changelog entry
    date=$(date +%Y-%m-%d)
    entry="## [{{version}}] - ${date}\n\n### Added\n\n### Fixed\n\n### Changed\n"
    sedi "s/^# Changelog$/# Changelog\n\n${entry}/" CHANGELOG.md
    echo "Bumped ${old} → {{version}}"
    echo "Updated: Cargo.toml, README.md, src/lib.rs, CHANGELOG.md"
    echo "Edit CHANGELOG.md to fill in release notes"

# Release: check → commit → tag → push → publish (run `just bump X.Y.Z` first)
release: check
    #!/usr/bin/env bash
    set -euo pipefail
    version=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    # Verify CHANGELOG has been filled in (not just template)
    if grep -q "^## \[${version}\]" CHANGELOG.md && grep -A3 "^## \[${version}\]" CHANGELOG.md | grep -q "^$"; then
        echo "⚠ CHANGELOG.md looks like a template — fill in release notes first"
        exit 1
    fi
    echo "Releasing v${version}..."
    git add -A
    git commit -m "chore: release ${version}"
    git tag "v${version}"
    git push origin main --tags
    cargo publish
    echo "Published kube-cel v${version}"

# Dry-run publish
publish-dry: check
    cargo publish --dry-run
