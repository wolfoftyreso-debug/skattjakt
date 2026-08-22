# Skattjakt — Engineering Decisions

Decisions that were not obvious, in the form the build order asks for: context,
options, choice, reason, trade-off. Anything settled by an ordinary default is
not here.

---

## D1. The rule set is unreviewed, and the engine enforces that

**Context.** The Swedish tax rules in `rules/se-ruleset.json` were
drafted by a language model. They carry statutory citations, and they are
plausible. They have not been checked by anyone qualified.

**Options.**

1. Ship them as ordinary rules and put a warning in the README.
2. Hold the rule set back until an adviser reviews it.
3. Make the review state part of the data and let the engine act on it.

**Chosen.** Option 3. Every rule carries `review`, which is either
`reviewed { reviewer, date }` or `awaiting_professional_review { note }`, where
the note says what specifically is unverified. `PipelineConfig
::require_reviewed_rules_for_identified` defaults to on, and an unreviewed rule
can never produce a finding with status `identified` — the best it reaches is
`verify`. Every rule shipped today is unreviewed, so the golden dataset asserts
that *no* finding in *any* case is presented as established, and `GET /v1/rules`
discloses the count.

**Reason.** A README warning does not travel with the output. A user reading a
finding needs to know its provenance at the point they read it, and a machine
check is the only kind that cannot be forgotten during a release.

**Revisited: the gate had one key and the key was a signature.** Option 3 as
built made an adviser's review the only way past the gate, which left the
product's correctness waiting on a person who had not been hired and whose
eventual signature nobody could re-check. So the rules and every constant were
re-cited against a registry of 24 primary sources — statute paragraphs at
`riksdagen.se`, Skatteverket's published positions — each carrying the claim the
rule makes of it and the operative strings that claim depends on.
The analysis worker fetches them on a six-hour sweep and checks; the same check
runs on demand as `skattjakt-analysis-worker verify-sources`. `verified` now
satisfies the gate alongside `reviewed`; `mismatch` overrides both and forces
`investigate`.

The retrieval state lives in the database rather than in the rule set, and that
is the part worth arguing. The rules are embedded in the binary on purpose — a
build must carry the rule set it was tested against. But a verification recorded
at build time can only ever be as current as the last build, and the law does
not change on our release schedule. So the *claim* is versioned with the code
and the *check* is not.

This is strictly more falsifiable than what it supplements. A signature cannot
be re-checked without repeating the review and says nothing once the law
changes; a retrieval is one command and a recorded hash, and the day
`30 kap. 5 §` stops saying 25 percent the check goes red on its own.

It does not replace the review, and the documents do not claim it does: a
retrieval establishes that the paragraph says what the rule assumes, not that
the rule computes the right base to apply it to.

**Status.** 0 of 24 sources retrieved. Every statute host is blocked by this
environment's egress proxy, so the machinery is proven against a real Postgres
and a real HTTP server
(`tests/integration/source-verification.sh`, 27 checks) and has never returned
`verified` for a real Swedish source. The registry says `unretrieved` rather
than being filled in from a language model's memory of the statute, which would
have made every downstream check meaningless while looking green.

**Trade-off.** Every finding currently reads as "needs verification", which
undersells the ones that are in fact solid. That is the right direction to be
wrong in. Flipping a rule to `reviewed` after a real review immediately lifts
its ceiling, with no code change.

---

## D2. Conditions evaluate in three-valued logic

**Context.** Onboarding is deliberately sparse (§3). Most profile answers are
unknown for most companies, and many rules depend on them.

**Options.**

1. Two-valued logic, treating unknown as false.
2. Two-valued logic, treating unknown as true.
3. Kleene three-valued logic, with unknown propagating.

**Chosen.** Option 3. `Condition::eval` returns `True`, `False` or `Unknown`.
Unknown propagates through `and`/`or`/`not` with the usual dominance rules, and
a rule that ends `Unknown` yields `RuleOutcome::Indeterminate` — a finding that
says "this could not be decided", plus the specific question that would decide
it.

**Reason.** "The company is not in a group" and "nobody has said whether the
company is in a group" have to lead to different outcomes. Collapsing the second
into the first is precisely how a confident wrong finding is produced, and it
would do so silently.

**Trade-off.** More states to handle at every call site, and the pipeline has to
decide what an indeterminate rule means for confidence and status. Worth it: the
unanswered questions become the "missing information" section, which is a
product feature rather than a limitation.

---

## D3. A rule whose trigger fact is absent does not apply, rather than being undecided

**Context.** Found while building the golden dataset. `interest_expense >
5 000 000` over a document with no interest line evaluated to `Unknown`, so the
interest-limitation rule produced an indeterminate finding on companies that
have no interest costs at all. Precision across the ten cases was 0.864.

**Options.**

1. Accept it — technically we *cannot* rule it out.
2. Suppress indeterminate findings entirely.
3. Guard each rule on the presence of its trigger fact.

**Chosen.** Option 3. Nine rules gained a leading `fact_present` condition, so an
absent trigger makes the rule `NotApplicable` instead of `Indeterminate`.
Precision went to 1.000 with recall unchanged at 1.000.

**Reason.** §30 is explicit that the goal is useful findings per analysis, not
findings per analysis. "You might have interest costs we cannot see" is true of
every company and helps none of them. The information is not lost — it surfaces
in `missing_information`, which is where a request for a document belongs.

**Trade-off.** A company that genuinely has a large interest cost on a page we
failed to parse now gets silence on that rule rather than a nudge. The unreadable
page is reported as a warning, which is the honest way to say it.

---

## D4. Costs are stored as positive magnitudes

**Context.** Also found via the golden dataset. Swedish income statements print
costs as negative. Rules written as `personnel_costs > 0` could therefore never
fire, and `depreciation < 30% of fixed_assets` was true for any negative
depreciation — a silent, wrong pass.

**Options.**

1. Write `abs` into every rule that touches a cost.
2. Store the sign as printed and require rules to know the convention.
3. Normalise costs to positive magnitudes in the canonical fact model.

**Chosen.** Option 3, via `FactKind::is_cost()`. The parser stays literal; the
normalisation happens once, in `build_fact_set`, and is documented there. The
verbatim `source_text` keeps the printed sign, so a reviewer still sees what the
document said.

**Reason.** Option 1 puts a correctness requirement in every future rule, and the
rule that forgets it fails silently rather than loudly — the worst failure mode
available.

**Trade-off.** A stored value can differ in sign from the line it was read from.
That is surprising for exactly as long as it takes to read the comment, and the
evidence card shows both.

---

## D5. The model never computes money

**Context.** §9 requires deterministic calculation where possible. The question
was how far to take it.

**Chosen.** Completely. `ImpactSpec` holds an expression tree evaluated by
`skattjakt-rules` over integer öre. The model is never asked for an amount, and
the prompts are tested to contain no digits at all — a rate, a threshold, a
statute reference or an amount appearing in a prompt fails the build.

**Reason.** A rule value in a prompt is a second copy of the rule set, and it is
the copy nobody updates when the law changes. The digit test is crude but it
cannot be argued with.

**Trade-off.** Prompts cannot use a concrete example to steer the model. In
practice the structured-output schema does that work.

---

## D6. Structured output is validated, not trusted

**Context.** The Messages API can constrain output to a JSON schema. It would be
reasonable to trust it.

**Chosen.** Validate anyway, in `skattjakt-model::schema`, including
`additionalProperties: false`. A response that does not satisfy the schema is a
`SchemaViolation` and the pass is treated as failed.

**Reason.** Schema enforcement is a property of a particular provider and model.
The provider abstraction exists precisely so that can change. A hand-written
~150-line validator is cheap insurance against a future provider that is looser.

**Trade-off.** The validator is a JSON Schema subset. Unsupported keywords are
ignored rather than half-enforced, which is the safe direction but means the
schemas must stay within the subset.

---

## D7. The model identifier is required configuration with no default

**Context.** §8 says not to hard-wire a model version, but the code has to name
one somewhere.

**Chosen.** `SKATTJAKT_MODEL_ID` is required and has no compiled-in fallback.
Starting without it runs the service rules-only and says so on `/ready`. A test
asserts no model identifier literal appears in the adapter source.

**Reason.** A default outlives its own accuracy. Naming the model is also part of
the audit trail — an analysis is reproducible only if you know what ran it, and
`model_runs.model` records it per call.

**Trade-off.** One more required environment variable, and a first-run failure
for anyone who skips the README. The readiness endpoint names the missing
variable, so the failure is self-explanatory.

---

## D8. Tenant isolation lives in Postgres, not in the query layer

**Context.** §20 requires that no data leak between companies. The usual
approach is a `WHERE company_id = $1` in every repository method.

**Chosen.** Row-level security. The application connects as `skattjakt_app`,
which is neither superuser nor table owner, every tenant table has
`FORCE ROW LEVEL SECURITY`, and each policy keys on
`current_setting('skattjakt.company_id')`. No tenant set means no rows.

**Reason.** The failure mode of the query-layer approach is one forgotten clause
in one method, and it fails open. With RLS the same mistake returns an empty
result. `scripts/test-tenant-isolation.sh` proves it against a real cluster
rather than asserting it — cross-tenant read by primary key, unfiltered select,
join, insert, update, delete, and audit tampering are each attempted and each
fail.

**Trade-off.** Every transaction must set the tenant, and a migration run by the
owning role bypasses the policies. Both are load-bearing operational facts, so
they are documented in the architecture note rather than left implicit.

---

## D9. A model outage degrades to a rules-only analysis

**Context.** Discovery and falsification both call a model. Either can fail.

**Options.** Fail the analysis; retry until it succeeds; continue without it.

**Chosen.** Continue. A failed pass records a `ModelRunRecord` with status
`failed` and contributes nothing. The rule engine produces evidence-backed
findings on its own.

**Reason.** The rule engine is the part that produces citable findings. A
rules-only result is complete, honest and useful. Failing the whole analysis
would be choosing nothing over something.

**Trade-off.** A user cannot tell from the result alone that discovery did not
run — only that fewer findings were corroborated. Surfacing the degradation in
the response is worth doing and is listed as a known gap.

---

## D10. Falsification cannot promote, only demote

**Context.** The skeptic pass returns a verdict per candidate.

**Chosen.** A verdict can reject a finding or raise its contradiction score. It
cannot raise confidence, and its absence is not treated as approval — a skeptic
that fails to run leaves the contradiction factor at zero, which is neutral, not
positive.

**Reason.** §11 asks the second pass to disprove. A pass that can also endorse
becomes a second advocate, and two advocates in a row is not verification.

**Trade-off.** A finding the skeptic actively confirms scores no higher than one
it merely failed to refute. Correct, if slightly unsatisfying.

---

## D11. Model-only findings exist but can never be actionable

**Context.** Discovery raises candidates no rule covers. Dropping them wastes the
model's contribution; presenting them risks exactly the hallucinated finding §30
warns about.

**Chosen.** They are surfaced with status `investigate`, zero monetary impact, and
`rule_match: 0` in the confidence factors — which the fail-closed caps turn into
a non-actionable score. They carry the question to ask, not an answer.

**Reason.** "Here is something odd, ask about it" is a legitimate product output.
"Here is something odd worth 120 000 kr" is not, when nothing computed that.

**Trade-off.** These findings will sit at the bottom of the list and some users
will never scroll to them. Acceptable: they are the weakest thing on offer.

---

## D12. Rust for everything, no Python workers yet

**Context.** §18 permits Python workers for PDF parsing and OCR.

**Chosen.** One Rust workspace. PDF text extraction uses the `pdf-extract` crate
behind a small interface.

**Reason.** A second language, runtime and deployment unit is a real cost, and
the interface it would sit behind is currently one function. The Swedish
statement parser — the part that determines extraction quality — is
string handling that Rust does as well as anything.

**Trade-off.** No OCR, so scanned PDFs produce no text. They are reported as
unreadable pages rather than silently contributing nothing, and OCR is where a
Python worker will genuinely earn its place.

---

## D13. Sign conventions, scale, and column order are parser concerns

**Context.** "12 500 000 11 200 000" on one line is two year columns or one
number, depending on convention.

**Chosen.** A thousands group must be exactly three digits, which ends the first
number at the column boundary; amounts are read only from lines that matched a
known label; the first amount on the line is the year under analysis; and
`Belopp i tkr` is detected and multiplied.

**Reason.** Restricting parsing to labelled lines is what stops page numbers,
dates and organisationsnummer from becoming facts — a false fact is worse than a
missing one, because it propagates into a calculation.

**Trade-off.** A layout that puts the comparison year first would be read
backwards. Detecting column order from headers is the obvious next improvement.

---

## D14. The tenant comes from the credential, never from the request

**Context.** Once persistence existed, every endpoint needed to know which
company it was acting for.

**Options.** A company id in the path or body; a tenant header; derive it from
the token.

**Chosen.** Derive it. An API token belongs to exactly one company, and
`Scope::Company(id)` is produced by the token lookup. There is no
`/v1/companies/{id}` — the only company route is `/v1/companies/me` — because a
path parameter could only ever name the same company or be refused, and offering
it invites a confused-deputy bug.

**Reason.** If the tenant were an input, every handler would have to check that
the caller may act for it, and the handler that forgot would be a cross-tenant
read. Deriving it means there is nothing to forget.

**Trade-off.** One token per company, so an advisor acting for several clients
holds several tokens. A membership model can be added later without changing the
principle.

---

## D15. Analyses run in the background and the client polls

**Context.** At high effort an analysis can legitimately run for minutes.

**Options.** Hold the request open; stream progress; return immediately and poll.

**Chosen.** Return `202` with an id, run the work on a task, and record each
stage transition in the database as it happens. `GET /v1/analyses/{id}` reports
`stage`, a Swedish label and a progress fraction — the same stage list the
product spec shows the user.

**Reason.** A minutes-long HTTP request dies to a load balancer, a proxy, or a
laptop lid. Recording stages in the database also means progress survives a
restart of the process that started the analysis.

**Trade-off.** The client has to poll, and a process that dies mid-analysis
leaves a job stuck at whatever stage it reached. A reaper for stale `running`
jobs is the obvious next step and is not written.

---

## D16. The interface is one file with no build step

**Context.** §25 asks for a minimal beta: "lägg in bokslutet, vi letar", not an
accounting system to learn.

**Options.** A React/Vite application; a server-rendered template; a single
static page.

**Chosen.** One HTML file with inline CSS and vanilla JavaScript, compiled into
the binary and served at `/`. No bundler, no package manager, no CDN — a test
asserts the page references no external host, so it works under a strict egress
policy and in an air-gapped deployment.

**Reason.** A build toolchain would have been the largest thing in the project by
file count and the only part needing a second language runtime in the container,
for a beta whose entire flow is five screens.

**Trade-off.** This does not scale to a real product surface. When the interface
grows past a handful of screens it should become a proper application; the API
is the contract, so that swap costs nothing on the server side.

---

## D17. A document's hash is verified every time it is read

**Context.** Analyses read stored blobs some time after they were written.

**Chosen.** `DocumentVersion::verify_hash` runs on every read in the analysis
path. A mismatch fails the analysis with a specific message rather than
analysing the bytes.

**Reason.** The whole evidence chain claims a value came from a specific
document version. If the bytes behind that version have changed — storage
corruption, a botched migration, tampering — every citation downstream is a lie.
Failing loudly is the only honest option.

**Trade-off.** Hashing on every read costs a pass over the file. For documents of
this size that is noise next to the model call in the same pipeline.

---

## Known gaps

Stated plainly, because a decisions document that only lists wins is not useful.

- **The rule set has not been professionally reviewed.** D1 contains the
  consequence, not a fix. Nothing here should be relied on for a filing.
- **2026 is not covered.** Prisbasbelopp and the 3:12 rules for 2026 could not be
  verified when the set was written, and a guessed constant is worse than an
  absent one. An analysis for tax year 2026 returns a 400 naming the gap.
- **No OCR.** See D12. A scanned PDF yields no text; the pages are reported as
  unreadable rather than silently contributing nothing.

- **No source has been retrieved.** Section 2 states it in full: 0 of 24, every
  statute host blocked by this environment's egress policy. Every constant in
  the rule set is transcribed from a citation nobody has fetched, and the
  `SourceState` ladder reports that honestly rather than hiding it.
- **No lawyer has read the terms or the consent wording.** The distance-selling
  text, the withdrawal notice and the consent checkbox were written against the
  statutes named in `SKATTJAKT_PAYMENTS.md` and reviewed by nobody qualified.
- **Swish has never been contacted.** The whole payment path — mutual TLS, the
  `PUT` to `paymentrequests`, settlement by `lookup()` — runs against a
  stand-in that speaks the Commerce API v2 wire format over real mutual TLS. It
  has never exchanged a byte with Swish, and will not until the Nordea merchant
  agreement exists.
- **Privatanalys has no rule to sell.** Every rule in `se-2025.1` is written for
  an aktiebolag, so the 29 kr private tier is listed with `available: false` and
  `POST /v1/orders` refuses it with 503. The gate is real — verified against a
  running shop — but the product does not exist until a private-individual rule
  is written.
- **Nothing is deployed.** The image builds and the manifests apply to a
  throwaway cluster; no environment runs this where a customer could reach it.

- **The depreciation rule read a heading, not a line.**
  `"materiella anläggningstillgångar"` mapped to the same fact the 30 %
  huvudregel was applied to, so a company that owned its premises was told it
  had an allowance the size of 30 % of its building. A run with 10,2 Mkr under
  that heading produced 630 360 kr that did not exist. `FactKind::Equipment` is
  now its own fact, the rule requires it, and golden case 11 is a
  property-owning company — the shape the first ten cases never covered.
- **The pension rule compared against the wrong base.** The frame in IL 28 kap.
  5 § is a share of cash pay; the rule took 35 % of `personnel_costs`, which
  also carries employer's contributions and runs about a third higher.
  `FactKind::Wages` now exists and the rule reads it.
- **The beloppsspärr guard missed standalone companies.** The loss rule's
  exception fired on `in_group`, but a spärr follows from an ownership change —
  most often a standalone company sold to a new owner. `ownership_changed` is
  now a profile question and the exception is an `any` of the two.
- **Deferred tax was counted as saved tax.** A periodiseringsfond allocation
  put 20,6 % of the headroom into the headline. `EffectKind` now separates the
  two: a deferral keeps its amount, gets its own line, and adds nothing to the
  total a reader takes as money to be had.
- **Two rules were point estimates with a band painted on.** ±15 % and ±10 %,
  measured by nobody. `ImpactSpec::Point` is gone from the type; every rule
  writes both bounds, and the low bound has to name a state of the world.

### Gaps that have since closed

Kept rather than deleted, because a gap list that quietly loses entries cannot
be trusted to still contain the open ones.

- **Blob storage was filesystem-only.** `crates/store/src/s3.rs` implements the
  same trait against S3 with hand-written SigV4;
  `tests/integration/s3-blobstore.sh` and `e2e-on-s3.sh` exercise it against a
  real MinIO, including that a tampered presigned URL is refused.
- **The Docker image had never been built.** Docker Hub still returns 403
  through this egress, but `mirror.gcr.io` does not, and the Dockerfile builds
  from there. `tests/supply-chain/inspect-image.sh` inspects the built image:
  13 MB, uid 65532, no shell, no baked credential.
- **Kubernetes had never been applied.** A k3s v1.31.2 cluster was brought up
  and all three overlays went through server-side apply. Section 5.1 of
  `SKATTJAKT_PRODUCT_SURFACE.md` records what it rejected and why.
- **Signed upload URLs were not implemented.** `apps/api/src/upload_routes.rs`
  issues a ticket and a presigned URL; the bytes never pass through the API.
