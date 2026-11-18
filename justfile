[private]
default:
    @just --list

# Node image for the live-apiserver recipes (parity, probe): the Kubernetes
# version the `tests/common` expectations were measured against (v1.36.1, cel-go
# v0.26.0). Pinned by digest (the multi-arch index from the kind v0.32.0 release
# notes) so a re-pushed tag cannot change the ground truth. Override with
# KIND_NODE_IMAGE.
kind_node_image := env("KIND_NODE_IMAGE", "kindest/node:v1.36.1@sha256:3489c7674813ba5d8b1a9977baea8a6e553784dab7b84759d1014dbd78f7ebd5")

# The GitHub repository `release` reads CI results from and releases to.
github_repo := "kube-rs/kube-cel"

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
    # Validated before anything is written; only digits and dots reach sed.
    version={{quote(version)}}
    semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
    if [[ ! "${version}" =~ ${semver_re} ]]; then
        echo "⚠ version must be X.Y.Z (digits only), got: ${version}"
        exit 1
    fi
    old=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
    if [[ ! "${old}" =~ ${semver_re} ]]; then
        echo "⚠ Cargo.toml version is not X.Y.Z: ${old}"
        exit 1
    fi
    old_minor="${old%.*}"  # e.g. 0.5.0 → 0.5
    new_minor="${version%.*}"  # e.g. 0.6.0 → 0.6
    old_minor_re="${old_minor//./\\.}"
    v_re="${version//./\\.}"
    sedi() { if [[ "$OSTYPE" == "darwin"* ]]; then sed -i '' "$@"; else sed -i "$@"; fi; }
    # Cargo.toml
    sedi "s/^version = \".*\"/version = \"${version}\"/" Cargo.toml
    # README.md + src/lib.rs — update version in dependency examples
    sedi "s/kube-cel = \"${old_minor_re}\"/kube-cel = \"${new_minor}\"/g" README.md
    sedi "s/version = \"${old_minor_re}\"/version = \"${new_minor}\"/g" README.md src/lib.rs
    # Changelog: date an existing `## [X.Y.Z] - Unreleased` section (notes
    # written as the work landed), otherwise add an empty one.
    date=$(date +%Y-%m-%d)
    if grep -q "^## \[${v_re}\] - Unreleased$" CHANGELOG.md; then
        sedi "s/^## \[${v_re}\] - Unreleased$/## [${version}] - ${date}/" CHANGELOG.md
    elif grep -q "^## \[${v_re}\]" CHANGELOG.md; then
        echo "⚠ CHANGELOG.md already has a dated section for ${version}; left it as it is"
    else
        entry="## [${version}] - ${date}\n\n### Added\n\n### Fixed\n\n### Changed\n"
        sedi "s/^# Changelog$/# Changelog\n\n${entry}/" CHANGELOG.md
    fi
    echo "Bumped ${old} → ${version}"
    echo "Updated: Cargo.toml, README.md, src/lib.rs, CHANGELOG.md"
    echo "Edit CHANGELOG.md to fill in release notes, then commit with sign-off:"
    echo "  git commit -s -am 'chore: release ${version}'   (or a descriptive message)"

# Run on the merged release commit (HEAD == origin/main, clean tree); never commits.
# Release: publish, then tag and push only that tag, then the GitHub release
release:
    #!/usr/bin/env bash
    # Land the version bump on main FIRST (`just bump X.Y.Z`, edit CHANGELOG,
    # `git commit -s`, merge). This recipe refuses a dirty tree and never creates
    # commits, so every release commit carries a DCO sign-off and a real message
    # (an old recipe made an unsigned `chore: release` commit, failing kube-rs DCO).
    #
    # Each git query is assigned before it is tested: a failed command
    # substitution in an assignment stops the script under `set -e`, while a
    # test of its (empty) output would carry on.
    set -euo pipefail
    # The tools the release needs, checked before anything else: python3 for
    # the date check and the release notes, gh (logged in) for the GitHub
    # release that ends it.
    for tool in python3 gh; do
        if ! command -v "${tool}" >/dev/null; then
            echo "⚠ ${tool} is not on PATH; the release needs it"
            exit 1
        fi
    done
    if ! gh auth status --hostname github.com >/dev/null 2>&1; then
        echo "⚠ gh is not logged in to github.com (gh auth login); the release ends with a GitHub release"
        exit 1
    fi
    # Pin the release to the commit this fetch brought (FETCH_HEAD; origin/main
    # stays stale where the fetch refspec does not cover main). That commit is
    # checked, published and tagged from its own checkout below, so moving HEAD
    # during the run cannot change what ships.
    git fetch --quiet origin refs/heads/main
    sha=$(git rev-parse --verify 'FETCH_HEAD^{commit}')
    head=$(git rev-parse --verify 'HEAD^{commit}')
    if [ "${head}" != "${sha}" ]; then
        echo "⚠ HEAD is not origin/main — release the merged release commit:"
        echo "  HEAD        $(git log -1 --format='%h %s' "${head}")"
        echo "  origin/main $(git log -1 --format='%h %s' "${sha}")"
        exit 1
    fi
    dirty=$(git status --porcelain)
    if [ -n "${dirty}" ]; then
        echo "⚠ working tree is dirty — commit the version bump first (git commit -s)."
        echo "  This recipe never commits, so the release commit keeps your sign-off."
        echo "${dirty}"
        exit 1
    fi
    manifest=$(git show "${sha}:Cargo.toml")
    # Here-strings, not pipes: under pipefail a reader that stops early
    # (`grep -q`, `exit`) can fail the writer with SIGPIPE.
    version=$(awk -F'"' '/^version = /{print $2; exit}' <<< "${manifest}")
    tag="v${version}"
    changelog=$(git show "${sha}:CHANGELOG.md")
    v_re=$(printf '%s' "${version}" | sed 's/\./\\./g')
    heading=$(grep -E "^## \[${v_re}\] - " <<< "${changelog}" || true)
    date="${heading##* - }"
    if [[ ! "${date}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] \
        || ! python3 -c 'import datetime, sys; datetime.date.fromisoformat(sys.argv[1])' "${date}" 2>/dev/null; then
        echo "⚠ no CHANGELOG.md section for ${version} dated with a real date (\`just bump ${version}\` dates it)"
        exit 1
    fi
    # Work left for this release is marked "before X.Y.Z" where it lives (the
    # divergence rows and tests that wait on cel-rust, the cel floor in
    # Cargo.toml).
    # git grep exits 1 when nothing matches; anything else is an error.
    if markers=$(git grep -n -i -F "before ${version}" "${sha}" -- . ':!CHANGELOG.md'); then
        echo "⚠ notes marked \"before ${version}\" remain; finish them first:"
        echo "${markers}"
        exit 1
    elif [ $? -ne 1 ]; then
        exit 1
    fi
    if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
        echo "⚠ tag ${tag} already exists locally"
        exit 1
    fi
    remote_tag=$(git ls-remote --tags origin "refs/tags/${tag}")
    if [ -n "${remote_tag}" ]; then
        echo "⚠ tag ${tag} already exists on origin"
        exit 1
    fi
    # CI must have passed on that commit: the ci workflow's `check` job and its
    # `apiserver parity (kind)` job, which `just check` does not cover. The
    # latest push run of ci.yml for the SHA counts, at its latest attempt. Both
    # jobs must be there, so a job renamed in ci.yml cannot pass silently.
    pick_run='
    import json, sys
    runs = [r for r in json.load(sys.stdin)["workflow_runs"] if r["head_sha"] == sys.argv[1]]
    if runs:
        r = max(runs, key=lambda r: (r["run_number"], r["run_attempt"]))
        print(r["id"], r["status"], r["html_url"])
    '
    judge_jobs='
    import json, sys
    status, url = sys.argv[1], sys.argv[2]
    jobs = dict((j["name"], j) for j in json.load(sys.stdin)["jobs"])
    required = ["check", "apiserver parity (kind)"]
    missing = [n for n in required if n not in jobs]
    if missing and status != "completed":
        sys.exit(print("job " + ", ".join(missing) + " not listed yet (the run is " + status + ")") or 2)
    if missing:
        sys.exit(print("the ci run " + url + " has no job named " + ", ".join(missing)
                       + "; if ci.yml renamed it, update the list in `just release`") or 3)
    failed = [n + ": " + str(jobs[n]["conclusion"]) for n in required
              if jobs[n]["status"] == "completed" and jobs[n]["conclusion"] != "success"]
    if failed:
        sys.exit(print("; ".join(failed) + " (" + url + ")") or 1)
    pending = [n + ": " + jobs[n]["status"] for n in required if jobs[n]["status"] != "completed"]
    if pending:
        sys.exit(print("; ".join(pending)) or 2)
    '
    ci_runs=$(gh api "repos/{{github_repo}}/actions/workflows/ci.yml/runs?head_sha=${sha}&event=push&per_page=100")
    ci_run=$(python3 -c "${pick_run}" "${sha}" <<< "${ci_runs}")
    if [ -z "${ci_run}" ]; then
        echo "⚠ no ci run for ${sha} yet (the push to main starts one); wait for it to finish, then rerun"
        exit 1
    fi
    read -r ci_run_id ci_run_status ci_run_url <<< "${ci_run}"
    ci_jobs=$(gh api "repos/{{github_repo}}/actions/runs/${ci_run_id}/jobs?per_page=100")
    if ci_message=$(python3 -c "${judge_jobs}" "${ci_run_status}" "${ci_run_url}" <<< "${ci_jobs}"); then
        ci_rc=0
    else
        ci_rc=$?
    fi
    case "${ci_rc}" in
        0) echo "CI passed for ${sha} (${ci_run_url})" ;;
        1) echo "⚠ CI failed for ${sha}: ${ci_message}"; exit 1 ;;
        2) echo "⚠ CI for ${sha} has not finished: ${ci_message}"
           echo "  wait for ${ci_run_url}, then rerun"; exit 1 ;;
        *) echo "⚠ ${ci_message}"; exit 1 ;;
    esac
    tmp=$(mktemp -d)
    trap 'git worktree remove --force "${tmp}/kube-cel" >/dev/null 2>&1 || true; rm -rf "${tmp}"' EXIT
    git worktree add --quiet --detach "${tmp}/kube-cel" "${sha}"
    # Release notes from that commit, written to a file before anything ships,
    # so a failure stops the release (an exit status inside `<(..)` is lost).
    notes="${tmp}/notes.md"
    (cd "${tmp}/kube-cel" && just release-notes "${version}") > "${notes}"
    if [ ! -s "${notes}" ]; then
        echo "⚠ the release notes for ${version} came out empty"
        exit 1
    fi
    echo "Releasing ${tag} from ${sha}..."
    # The gate runs on the commit being released, after the cheap refusals.
    # Publish before tagging: a failed publish leaves no tag anywhere.
    (cd "${tmp}/kube-cel" && just check && cargo publish)
    git tag "${tag}" "${sha}"
    # Only this tag is pushed: an explicit refspec (never --tags), and
    # --no-follow-tags so `push.followTags` cannot add other annotated tags.
    if ! git push --no-follow-tags origin "refs/tags/${tag}"; then
        echo "⚠ published ${version}, but pushing ${tag} failed; retry with:"
        echo "  git push --no-follow-tags origin refs/tags/${tag}"
        echo "  just release-notes ${version} > notes.md && gh release create ${tag} --title ${tag} --notes-file notes.md --verify-tag"
        exit 1
    fi
    # GitHub release: notes reflowed to one line per paragraph/bullet (GitHub
    # renders single newlines as <br>, so hard-wrapped CHANGELOG text breaks
    # mid-sentence otherwise).
    if ! gh release create "${tag}" --title "${tag}" --notes-file "${notes}" --verify-tag; then
        echo "⚠ published ${version} and pushed ${tag}, but creating the GitHub release failed."
        echo "  Do not rerun \`just release\` (the tag exists now). From the release commit, retry with:"
        echo "  just release-notes ${version} > notes.md && gh release create ${tag} --title ${tag} --notes-file notes.md --verify-tag"
        exit 1
    fi
    echo "Published + released kube-cel ${tag}"

# Print GitHub-friendly (reflowed) release notes for VERSION from the CHANGELOG
release-notes version:
    #!/usr/bin/env python3
    # Each paragraph/bullet is reflowed onto a single line, because GitHub
    # renders single newlines as <br> (hard-wrapped CHANGELOG text would break
    # mid-sentence). Used by `release`; also handy for re-editing a release:
    #   just release-notes X.Y.Z > notes.md && gh release edit vX.Y.Z --notes-file notes.md
    import re, sys
    version = {{quote(version)}}
    if not re.fullmatch(r'\d+\.\d+\.\d+', version):
        sys.exit(f'version must be X.Y.Z, got: {version}')

    def fence_open(line):
        """The (char, length) of a ``` or ~~~ fence opening on this line."""
        m = re.match(r'\s*(`{3,}|~{3,})', line)
        return (m.group(1)[0], len(m.group(1))) if m else None

    def fence_closes(line, fence):
        m = re.fullmatch(r'\s*(`{3,}|~{3,})\s*', line)
        return bool(m) and m.group(1)[0] == fence[0] and len(m.group(1)) >= fence[1]

    # The section runs from its heading to the next `## [` heading, both
    # recognised only outside fenced code blocks. A fence left open anywhere
    # would move where sections start and end, so it is an error; the whole
    # file is scanned for one.
    sec, inside, found, fence, fence_line = [], False, False, None, 0
    for n, l in enumerate(open('CHANGELOG.md', encoding='utf-8').read().splitlines(), 1):
        if fence is None and l.startswith('## ['):
            inside = not found and l.startswith(f'## [{version}]')
            found = found or inside
            continue
        if fence is not None:
            if fence_closes(l, fence):
                fence = None
        else:
            fence, fence_line = fence_open(l), n
        if inside:
            sec.append(l)
    if fence is not None:
        sys.exit(f'CHANGELOG.md:{fence_line}: a code fence that is never closed')
    if not found:
        sys.exit(f'no CHANGELOG.md section for {version}')

    out, cur, fence = [], None, None
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
        if fence is not None:
            out.append(l)
            if fence_closes(l, fence):
                fence = None
            continue
        if fence_open(l):
            flush(); out.append(l); fence = fence_open(l); continue
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
