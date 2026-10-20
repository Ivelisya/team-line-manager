# Handoff: 团队线路管理（team-ops）全面重建 — 2026-09-23

Session ran as Opus 5.5 (user asked for Opus 5.5 throughout; subagents inherit). Ultracode was on (use Workflow for substantive work). Chat language: Simplified Chinese.

## 1. What the user wants (condensed; full standing rules live in memory)

Optimize the internal admin console https://38.58.57.22/admin/home (Komari 1.5 Go + team-ops Python + LuminaPlus home + vanilla ops pages, glued with iframes): performance, CPU usage on servers, UI consistency/polish/interaction, batch node onboarding (DD → SS → attach relay), stronger monitoring & management with ZERO regression. Rust rewrite allowed (incl. Komari), reuse over reinvention, push-based monitoring, edge-distributed (central = 线路机2 master, other relays = sub-servers), K8s-like patterns OK.

Read these memory files first (they hold the durable rules and corrections; don't restate them from this doc):
- `~/.claude/projects/-Users-kismet-Desktop-leap/memory/team-ops-dev-flow.md` — dev flow (Desktop → a395 staging → production), scope rules (internal-only, no differentiators/taint history, fold production risks into the new architecture instead of hotfixing, plan catalog, IP intelligence, no rented VPS — borrow a landing node, non-destructive by default).
- `.../memory/ui-style-high-bar.md` — **keep the ORIGINAL look** (LuminaPlus card style) and only beautify/unify; the user strongly rejected 7 new visual concepts ("好丑"). Don't treat ambiguous short replies as decisions.
- `.../memory/team-ops-fleet.md` — fleet, customers (cross-border e-commerce teams on OpenClash/mihomo soft routers) → Xray REALITY must stay mihomo-compatible.
- `.../memory/browser-prefer-chrome.md` — use Claude in Chrome, never the built-in browser.

## 2. Workspace and artifacts (all under `~/Desktop/team-ops-v2/`, not a git repo)

- `upstream-snapshot/` — pulled production code (release tree, running plugin, native admin build, server-config.txt). Only source of the old system.
- `eval/REPORT.md` + `REVIEW-skeptic.md`, `REVIEW-completeness.md` — Rust-rewrite performance evaluation (done). Headline: ~89% of CPU savings come from architecture, Rust adds a few points; measurements taken at peak hours, include steal.
- `research/` — inventories (`inventory/{home,ops,native,api}.md`), UI redesign brief (`design/redesign-brief.md` — use §1 audit and §4 format/term rules only, NOT its new visual directions/IA), onboarding (`onboarding/{current-flow,building-blocks,pipeline-spec}.md`), critiques (`critique/{ui,pipeline}.md`).
- `architecture/` — research tracks `research/01..09*.md` (monitoring, proxy panels, control plane incl. k3s measured, storage, runtime/frontend, data plane, capabilities, IP intelligence, competitors), `KNOWN-RISKS.md` (R1–R13, all to be solved by the new architecture), three designs, `ARCHITECTURE-v1.0.md` (chosen: design A — edge-autonomous single-writer control plane; Rust `fleetd`/`relayd`/`nodeagent`; SQLite per data shape + Litestream; self-built Xray v26.9.9 with reality pinned to e1986a4, blackhole default outbound, API on UDS; ssserver on nodes; React SPA + SSE), `REVIEW.md` (7 high: H1 telemetry flow-control deadlock, H2 outbox seq reuse, H3 SSH CA = fleet root, H4 probes depend on central, H5 relays lack plan/quota/due/cost, H6 cert time bombs, H7 unrealistic schedule), `ARCHITECTURE.md` (should now be v1.1 from the revision agent — verify), `plan-catalog/catalog.md|json` (6 canonical plans; 7 open questions Q1–Q7 for the user).
- `prototype/` — `data/snapshot.json` (anonymized real data + IP enrichment), `data/plan-catalog.json` (aliases stripped — user said discard original plan strings), `shared/*.js` (format/data/live/table/keys/charts, uPlot vendored), `original/` (DNA docs `dna-home.md`, `dna-ops-native.md`, a local replica harness, and **79 baseline screenshots of the ORIGINAL UI** in `original/shots/`), `original-refined/` (the refined-original prototype being built), `concepts/` (7 rejected concepts + `index.html` gallery — keep only as reference, do not pursue).
- Local static server: `http://127.0.0.1:5199/` serving `~/Desktop/team-ops-v2/prototype/` (python http.server, may need restart).
- Workflow scripts: `~/.claude/projects/-Users-kismet-Desktop-leap/437b227d-7c23-499f-a7c9-13152f4ee792/workflows/scripts/` (latest: `revision-v2.js`, `refine-v2.js`). Journals: `.../subagents/workflows/wf_*/journal.jsonl`.

## 3. In-flight work at handoff (verify state first)

1. **Architecture revision** — task `wsdfxe8un`, script `revision-v2.js`, resumed from run `wf_f17a3a80-d03`. Stage 1 (revise → v1.1) had completed; resumed run should replay it from cache and run verify (fix-by-fix + fresh re-attack) → finalize v1.2 + `architecture/SUMMARY-for-user.md`. The resume check was interrupted by the user, so **confirm the run started and hit cache** (journal should show `verify:*` started, not a new `revise:v1.1`). Staging rule injected: no paid VPS; a395 + one borrowed low-traffic landing node not mapped to any team site; recommend 1–2 candidates for the user to confirm; DD only on a node the user confirms idle.
2. **Original-style refine prototype** — task `wpfowicvx`, script `refine-v2.js`, run `wf_b4dff03f-481`: DNA (done, cached) → spec → foundation+home → 5 pages in parallel (dashboard, nodes+drawer, lines, behavior, NEW onboarding page in original style) → 3 critiques (fidelity/consistency/craft) → refine → before/after `original-refined/compare.html` + `final/overview.png`. Plans shown by canonical name only.

When both finish: present `SUMMARY-for-user.md` + the before/after comparison to the user (send screenshots one per call or a single overview image — the side panel only rendered the first of several files last time; also `open <url>` works to show pages in their default browser).

## 4. Pending user decisions / open questions

- Plan catalog Q1–Q7 (`architecture/plan-catalog/catalog.md`), e.g. MX node traffic unlimited vs 1024 GB, ATTVPS 标准版 RAM, canonical names OK.
- Which landing node to borrow for experiments (candidates will come from the revision workflow).
- Customer-site router management (device IP range → node mapping, tunnel/config monitoring): proposed P2, user hasn't answered.
- Architecture approval (v1.2) and the phased plan/first deliverable.

## 5. Gotchas learned this session

- Don't `SendMessage` to a running workflow subagent: it resumes as a separate session and the workflow never receives the result (happened with `profile:team-ops`).
- Workflow resume only hits cache when earlier `agent()` calls keep the same prompt AND order; to change later stages, edit only later prompts, wait for the earlier stage to finish (background `until` loop on journal.jsonl), then TaskStop + resume. Editing shared CTX invalidates everything.
- Subagents sometimes can't reach Claude in Chrome; they fell back to headless Chrome for local pages (acceptable). The main session's Chrome extension was disconnected late in the session.
- Production is read-only for analysis; one agent accidentally created then removed a `.pyc` on line3 — keep "no writes outside /tmp/teamops-eval" in prompts. `/tmp/teamops-eval/` on central/line1/line3 still holds py-spy and samplers (cleanup pending, harmless).
- a395 (`ssh 10.172.17.213`, user kismet): work only under `~/team-ops-eval`, `~/arch-eval`; never touch the Leap dev stack; bind 127.0.0.1.
- Subagents may be blocked from writing report files; save returned text yourself (done for `eval/REPORT.md`).
- Measurement windows on production: cap at 5 minutes per agent (user instruction).

## 6. Suggested skills for the next agent

- `workflow-authoring` — before authoring/resuming any Workflow script (ultracode is on).
- `artifact-design` (and `Artifact` quickstart) — only if publishing the summary/comparison as a page for the user.
- `mattpocock-skills:tdd` — when implementation of fleetd/relayd/nodeagent starts (golden tests for the "invisible semantics" listed in `architecture/research/07-capabilities.md`).
- `run` — to launch/screenshot the prototype or later the real app.
- `code-review` / `security-review` — once code exists, before anything goes to a395 or production.
