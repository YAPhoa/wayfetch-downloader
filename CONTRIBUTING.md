# Contributing to Wayfetch Downloader

Contribute working behavior, reproducible tests, and documentation someone will maintain. A small, complete change is more useful than broad scaffolding or a dossier of agent activity. These expectations apply to human and AI-assisted contributions.

## Where guidance lives

| Document | Responsibility |
|---|---|
| This guide | Choosing, implementing, testing, and submitting contributions |
| [AGENTS.md](AGENTS.md) | Agent coordination, repository hygiene, verification reporting, and temporary-output retention |
| [Architecture brief](docs/architecture.md) | Engine boundaries, product requirements, decision register, and acceptance gates |
| `README.md` and existing CI | Actual supported setup, build, run, and check commands |

The canonical architecture brief lives at `docs/architecture.md`; do not create another copy. Read only the sections relevant to the change. Apply the hygiene and retention rules in `AGENTS.md` to every contribution; this guide does not establish a competing policy.

Architecture proposals are not proof that packages, commands, features, or supported platforms already exist. Inspect the current checkout before relying on them.

## Choose a bounded change

Use the existing issue or task thread when one exists. A small bug fix, test, or documentation correction does not need a new proposal or mandatory planning document. For a larger change, describe the intended outcome, affected interfaces, exclusions, and validation in the existing discussion before implementation.

Coordinate changes to public engine/protocol contracts, migrations, root manifests and lockfiles, UI transport schemas, or security boundaries with the integration owner. Keep unrelated refactoring, dependency upgrades, formatting sweeps, and new features out of the same contribution. Do not create an abstraction without a concrete use in the accepted scope.

Use one branch or assigned worktree per independently owned change. Concurrent contributors must not edit overlapping paths in the same working directory. Preserve other contributors' uncommitted work; do not reset it to make a check pass or the tree appear clean.

## Set up from the actual repository

Use the Rust toolchain, package-manager choice, lockfiles, and native dependencies specified by the checkout and its CI. Avoid unrequested upgrades. Install only what the selected target needs: an engine or server contribution should not require a desktop toolchain or WebView. Frontend build tooling is separate from the server's runtime requirements.

Follow existing setup and test entry points. The illustrative Rust checks in `AGENTS.md` are not a substitute for inspecting real package names and feature combinations. If bootstrap has not established runnable commands, add and verify the necessary commands in the existing `README.md` as part of that work; do not publish guessed commands or success-only placeholder scripts.

Use a separate development state directory and dedicated test download/extraction roots. Never point tests at a real queue, production database, user archive collection, or live deployment. Synthetic/local fixtures are the default. Provider credentials, paid services, live traffic, and deployment changes need explicit authorization.

## Implement within the shared-engine architecture

Keep scheduling, job state, authorization invariants, transfers, and recovery in the shared Rust engine. Server HTTP handlers, desktop IPC handlers, CLI commands, and UI components remain thin clients or adapters. Do not implement the same behavior separately for browser and desktop.

The server must serve its complete bundled UI without a vendor portal or runtime asset CDN. Desktop packaging must embed the same engine rather than conceal a JVM, third-party download-manager, or daemon sidecar. Preserve the independent engine/server build and early embedding proof required by the architecture brief.

Follow existing code style and error conventions. Validate untrusted input at its boundary, propagate actionable errors, and make resource ownership, cancellation, and cleanup explicit. Bound both item counts and bytes. Explain any necessary `unsafe` code and cover its safety assumptions. Do not weaken TLS, path checks, isolation, quotas, tests, or error handling merely to obtain a passing result.

For behavior changes, update the applicable tests and existing decision entry. Preserve existing decision IDs. A separate design document is warranted only for a substantial enduring topic that cannot reasonably fit an existing document—not for every implementation choice.

## Test the claim you are making

Run the existing formatting, lint, and relevant test/build checks for the affected code. Use the agreed feature/platform matrix rather than indiscriminately enabling every feature. Run broader integration checks when shared contracts or dependency changes affect other components; an unrelated wording correction does not require every expensive release gate.

| Change | Expected verification |
|---|---|
| Bug fix | A focused regression test that exposes the defect before the fix where practicable, then passes with it |
| Transfers, state, or extraction | Relevant interruption, cancellation, corruption, resource-bound, and recovery cases from the architecture brief |
| API, UI, or desktop boundary | Real shared-engine behavior, permission failures, event resync, and affected HTTP/IPC conformance; mocks alone are insufficient |
| Provider integration | Sanitized request-sequence fixtures, explicit supported modes and failure mapping; separately authorized live checks before claiming live verification |
| Packaging or performance | Actual target build/runtime checks; measured workload, configuration, environment, and complete process-accounting boundary |

Keep fixtures small, sanitized, and consumed by maintained tests. Generate large datasets in temporary test storage. Do not commit credentials, signed URLs, private downloads, raw provider-page dumps, or entire third-party archives merely to make a test convenient.

Report exact commands, results, and the tested commit or dirty-tree state. Distinguish a code failure from unavailable dependencies, unsupported platforms, missing credentials, and tests not run. A skipped test is not a pass; fixture success is not live-provider verification. Never claim a memory improvement without an actual comparable measurement.

## Keep documentation and verification output lean

The default implementation contribution adds **no report, plan, handoff, or evidence files**. Put routine results in the task response or PR/CI output. Update existing setup, architecture, or capability documentation only when its durable content changes.

Follow `AGENTS.md` for scratch locations, retention, safe cleanup, and the single task-status authority. Do not duplicate an issue tracker in Markdown, append a diary to policy files, create timestamped “final” copies, or split the architecture register into a file per decision. Ignored output still requires cleanup.

A new persistent non-code file needs an ongoing consumer and a reason an existing file is insufficient. Explain that briefly in the PR, not in another justification document. Necessary license notices, maintained test fixtures, explicitly requested documents, and compact release records remain valid deliverables.

## Submit and review

Submit one reviewable outcome. Describe the problem and user-visible result, link the existing issue when relevant, and identify breaking behavior, migration effects, new dependencies, or security-sensitive changes. Keep commits understandable; do not rewrite shared history or invent a commit reference for uncommitted work.

Use the five-field handoff in `AGENTS.md`—**Changed, Verified, Not verified, Remaining, Cleanup**—in the PR body or task response. Attach compact relevant results, not raw logs. Do not create a repository handoff file or an additional PR-template file for this purpose. Temporary screenshots may support a UI review; they are not permanent source assets unless a maintained test consumes them.

Before requesting review, inspect the diff and newly added paths, run applicable checks, and remove owned disposable output and abandoned debug changes. Preserve the minimum useful reproducer for an unresolved failure under the retention policy; cleanup must not conceal a known defect. Stop only processes you own, and leave unfamiliar or irreplaceable files untouched.

Reviewers should assess correctness, shared-engine boundaries, security/recovery, tests, resource limits, and every unexplained new file. Resolve findings in the existing review thread. Recheck the fix and affected risks, then close the review; do not generate review-of-review reports or reopen accepted work without a concrete defect or changed requirement.

Passing a contribution's checks does not certify every provider, release gate, or operating system. The integration owner accepts the relevant milestone using its actual requirements. Remove disposable worktrees only after integration or deliberate abandonment, with no active user or uncommitted work at risk.

## Dependencies, provenance, and sensitive reports

Follow the repository's chosen license and applicable per-file terms. Record provenance and required notices for material actually incorporated. If the project license or reuse terms are unresolved, raise that before accepting external code; this guide does not choose a license, impose a contributor agreement, or authorize redistribution.

Do not assume that translating third-party code to Rust removes reuse obligations, copy provider application identities or service credentials, or describe source-informed work as clean-room. Keep reference checkouts outside shipping sources. A new dependency needs a concrete purpose and review of its features, license, maintenance/security status, and effect on headless builds.

AI-assisted work receives the same review and test requirements. Contributors remain responsible for understanding submitted code, checking provenance, and verifying claims. Do not send secrets or private provider responses to external tooling without authorization; no agent transcripts or per-task AI activity reports are required.

For an ordinary bug, supply the tested revision, relevant environment, minimal sanitized reproduction, and expected versus actual behavior. For a security issue, use the project's published private reporting route when available. If none is published, request a private maintainer contact without posting exploit details or credentials publicly. Do not invent a reporting address or test against systems without authorization.

**A contribution is complete when its scoped behavior works, applicable checks are reported honestly, necessary maintained documentation is current, and disposable task output is cleaned up.**
