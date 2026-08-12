# Skattjakt — the Monte Carlo layer

A general probability and simulation layer: define uncertain inputs, calculate
outputs over them, and read the whole distribution rather than a single figure.

It is not a chart component and it is not a feature of one screen. Nothing in
`skattjakt-simulate` knows about tax, about companies, or about HTTP; a run is a
pure function of a specification, a seed and an iteration count. That is what
makes it reusable by anything in the product that has to reason about an
uncertain number, and what makes the whole engine testable without a process.

---

## 1. The conflict with the rest of this product, resolved first

Skattjakt's oldest domain rule is that **money is a `MoneyRange`** and that no
type in the product can express a single-figure tax saving. A simulated P50 is a
single figure. So the two have to be placed in relation to each other before
anything else, because the tempting shortcut here is also the one that would
undo the rule the product is built on.

**The resolution: a simulation is not evidence, and nothing it produces may
become the amount on a finding.**

| | An `opportunity` | A simulation |
|---|---|---|
| Comes from | The company's own documents plus a versioned rule | Assumptions somebody typed in |
| Needs | ≥1 document value **and** ≥1 cited rule | A model and a seed |
| Says something about | The company's accounts | The model |
| Amount | `MoneyRange`, öre, never a point | A distribution; percentiles are read off it |

They live in separate tables, are read through separate endpoints, and no column
in `simulation_*` is referenced by `opportunities`. That separation is asserted
by the schema and stated in the migration, because the day someone wires a
simulated P50 into a finding is the day the evidence gate stops meaning
anything.

Read the other way round, a distribution is a *stronger* statement of
uncertainty than an interval, not a weaker one. It is the right tool for "what
might this cost", and the wrong tool for "what does the annual report say".

---

## 2. The layers

```
   Web / iOS / Android
            │  one contract, /v1/simulations
      ┌─────▼─────────┐
      │  API          │  authn → authz → validate → decide where to run
      └─────┬─────────┘
            │ small: spawn_blocking, answer in the request
            │ large: durable job on Postgres
      ┌─────▼─────────┐        ┌──────────────────┐
      │  Worker       │◄──────►│  skattjakt-jobs  │  leases, retries, cancel
      └─────┬─────────┘        └──────────────────┘
            │
      ┌─────▼──────────────────────────────────────────┐
      │  skattjakt-simulate                            │
      │  rng → distribution → expr → engine            │
      │        → stats → sensitivity → convergence     │
      │        → shape                                 │
      └─────┬──────────────────────────────────────────┘
            │  statistics, sensitivity, convergence, chart payload
      ┌─────▼─────────┐
      │  PostgreSQL   │  9 tables, row-level security forced on all of them
      └───────────────┘
```

| Module | Owns |
|---|---|
| `rng` | The deterministic generator, and one stream per input |
| `distribution` | Eleven distributions: validation, sampling, analytic moments |
| `expr` | The expression language an output is written in |
| `spec` | Inputs, outputs, constraints, compilation, the model hash |
| `engine` | The run loop, cancellation, quality checks |
| `stats` | Percentiles, moments, probabilities, confidence intervals |
| `sensitivity` | Correlation, rank correlation, contribution to variance |
| `convergence` | Whether the run has settled |
| `shape` | Histogram, density and CDF — the visualisation payload |

---

## 3. Reproducibility, which drives most of the design

Section 12 asks that the same seed, the same inputs and the same engine version
reproduce a result. Four decisions follow from taking that literally.

**The generator is written here rather than taken from a crate.** A dependency
makes the promise depend on a version range: `rand` changed its `StdRng`
algorithm between major versions, and a project that had persisted seeds would
have found its old runs unreproducible after a routine `cargo update`. The
sequence a seed produces is part of this system's contract, so it lives in this
system's source, pinned by a test that fails if a byte of it changes.

**Every input draws from its own stream**, seeded from the run's seed and the
input's identifier. Adding, removing or reordering an input therefore leaves
every other input's numbers untouched. A single shared stream would make every
historical run irreproducible the first time somebody added a variable — which
is the first thing anybody does to a model.

**Seeds are transported and stored as decimal strings.** A JSON number silently
loses precision above 2^53, so a client would round-trip a seed, send it back,
and get a different simulation with no error anywhere. Postgres has no unsigned
64-bit integer either; a `BIGINT` would work by bit-casting, at the cost of an
audit record where half the seeds display as negative numbers that do not match
what the API returned.

**A run references a model *version*, never a model.** Editing a model appends a
version; earlier runs keep meaning what they meant. Without this the seed
recorded beside an old result would reproduce something else entirely.

`ENGINE_VERSION` is bumped whenever a change could alter the numbers a given
seed produces. A result from an older engine is marked unreproducible on this
one rather than silently recomputed into different figures.

---

## 4. The distributions

Eleven, each with the same four obligations: a name, parameter validation, a
sampler, and its own mean and variance **analytically**. The last is not
decoration — it is what the statistical tests check the samplers against, and a
sampler tested only against itself is a sampler that can be confidently wrong.

| | Parameters | For |
|---|---|---|
| Normal | `mean`, `std_dev` | Symmetric variation. Can go negative — wrong for volumes and prices |
| Lognormal | `log_mean`, `log_std_dev` | Non-negative, multiplicative. Parameters are those of the *underlying normal* |
| Uniform | `low`, `high` | Only a floor and a ceiling are known |
| Triangular | `low`, `mode`, `high` | Three-point estimation |
| Beta | `alpha`, `beta`, `low`, `high` | Bounded, asymmetric. Scaled rather than left on [0,1] |
| Exponential | `rate` | Time until an event |
| Poisson | `lambda` | Counts in an interval |
| Bernoulli | `p` | One yes or no |
| Binomial | `trials`, `p` | Successes out of n |
| Discrete | `values`, `weights` | Named scenarios |
| Custom | `points` | An arbitrary shape, given as cumulative points |

**Invalid parameters are never silently repaired.** A triangular whose mode is
outside its range, a beta with `alpha = 0`, discrete weights summing to zero:
each is rejected with a message naming the parameter. A simulation that fixes
its own inputs produces a number nobody asked for and everybody believes.

Two sampler choices worth stating. **Poisson** uses Knuth below `lambda = 10`
and Hörmann's transformed rejection above it — Knuth costs one uniform per unit
of `lambda`, so at `lambda = 1000` and a million iterations it would draw a
billion of them. **Binomial** sums Bernoullis up to 128 trials and uses BTRS
above, with the standard reflection so the algorithm only ever sees `p ≤ 0.5`.

---

## 5. The calculation model

An output is an expression over the inputs and over any output declared before
it, which is what makes `profit = revenue - costs` work while making a circular
reference an unknown-name error rather than an infinite loop.

```
revenue  = customers * (1 - churn) * average_revenue + if(wins_contract, 250000, 0)
costs    = fixed_costs + revenue * variable_cost_rate + incidents * incident_cost
profit   = revenue - costs
```

Operators `+ - * / % ^`, comparisons, `and`/`or`/`not`, and the functions
`min max abs sqrt exp ln log10 floor ceil round clamp` plus `if(c, a, b)`.

**A parser rather than an embedded scripting language.** A general interpreter
would be less code and would let a stored expression read files, loop forever or
allocate without bound — in a worker process holding a database connection, on
input a customer controls. This language has no loops, no assignment, no I/O and
no way to name anything the model did not declare. It terminates because it
cannot do otherwise. Nesting is bounded at 64 levels so a parenthesis bomb from
an API request is a 422 rather than a stack overflow.

**Names resolve to slots at compile time.** A hash lookup per variable per
iteration would be twelve million hash lookups in the inner loop of a
million-iteration run.

`if` and `and`/`or` short-circuit, so `if(x > 0 and 1/x > 2, …)` does not divide
by zero to decide that it should not have.

---

## 6. What the engine refuses to do

Section 11: never silently produce a result that is statistically or numerically
invalid.

| Condition | What happens |
|---|---|
| A NaN or infinity in any output | The **run fails**, naming the output and the iteration |
| A NaN condition in an `if` | The iteration becomes NaN, so the above applies — no branch is guessed |
| A constraint no draw satisfies | Rejected at compile time if it excludes the support; otherwise the run fails after 1 000 attempts |
| Iterations outside [100, 10 000 000] | 422 |
| iterations × outputs above 24 million | 422, naming the numbers |
| Zero variance everywhere | A valid result: correlations are `null`, the histogram is one bar, and the sensitivity report carries a note saying why |
| Not converged | A result **with a warning attached to it**, in Swedish, ready to display |

Rejected and clamped draws are counted and returned in `quality`, with a warning
when they exceed a threshold: a run where a third of the draws were rejected by
a constraint is a run whose input distribution is not what its author thinks.

---

## 7. Reading the numbers

**Percentiles** are computed exactly, from the sorted samples, by linear
interpolation between order statistics — the definition R calls type 7 and NumPy
uses by default. Stating which one matters: the seven common definitions
disagree by up to a whole rank, and a P90 that moves when the tool changes is a
P90 nobody can act on.

**Summation is Welford's, not a running total.** On ten million values of widely
different magnitude a naive sum loses low-order bits steadily, and the variance
computed as `E[x²] − E[x]²` can come out *negative* — whose square root is a NaN
standard deviation from perfectly good data.

**`mean_confidence_interval_95` is the interval for the mean** — this run's
sampling error — not the spread of the outcomes. The two are indistinguishable
once they are numbers on a screen, so the field name carries the distinction.

**`probability_of_target` is `null` when no target was set**, never zero, and it
is serialised as an explicit `null` rather than omitted. An absent key reads as
`undefined` in a client, renders as nothing, and the difference between "no
target" and "cannot happen" disappears at the last step.

### Sensitivity

Three measures, because each fails differently. **Pearson** measures a
straight-line relationship and is the one that misleads: an input with a strong
but curved effect — a threshold, a cap, anything with an `if` — can show a
correlation near zero while dominating the outcome. **Spearman** survives
non-linearity and is the one to read when they disagree. **Contribution to
variance** normalises the squared rank correlations into shares that sum to one,
which is the form the question is usually asked in.

It assumes the inputs are independent of one another. True here, because each
draws from its own stream — and stated, because it would not be true of a model
with correlated inputs.

An input the output's expression never reads is reported as `referenced: false`
with no correlation at all. A finite sample always shows *some* spurious
correlation, and reporting it would be reporting noise as a finding. The
reference check is transitive: `profit` never names `customers`, but it names
`revenue`, which does.

### Convergence

The mean, median, P10 and P90 as they stood at 1 000, 5 000, 10 000 … iterations,
each computed from a sorted copy of that *prefix* — the numbers a shorter run
would genuinely have produced, rather than percentiles of the full sorted array,
which would use information from iterations that had not happened yet.

Movement is measured relative to the larger of the values and the distribution's
spread. Against the value alone, a statistic near zero is never stable: a mean
wandering between 0.001 and 0.002 on a standard normal is a 100% change and a
completely converged one.

Tails are held to a looser tolerance than the centre (3% against 1%), because a
P90 is estimated from a tenth of the sample and converges roughly three times
more slowly. One tolerance would either pass unstable tails or fail on stable
medians.

---

## 8. Memory, and the three kinds of data

Section 16 asks for raw data, statistical aggregates and visualisation data to
be kept apart. They are, and only two of the three are stored.

| | Size at 1M iterations × 3 outputs | Where it lives |
|---|---|---|
| Raw samples | ~24 MB | Memory, for the length of the run. **Never stored** |
| Statistics | ~2 KB | Columns, queryable across runs |
| Chart payload | ~15 KB | One JSONB row per output |

The raw samples are reproducible from the seed at any time. Persisting eighty
megabytes per run to avoid a two-second recomputation is the wrong trade in
every direction, and sending them to a browser would be a 76 MB response for a
chart 900 pixels wide.

**Output samples are kept in full** during a run, because exact percentiles are
worth eighty megabytes for a number somebody is going to make a decision with. A
t-digest would save the memory and give approximate percentiles.

**Input samples are kept only for the first 100 000 iterations**, for the
sensitivity analysis. The standard error of a correlation from n samples is
about `1/√n`, so a hundred thousand gives ±0.003 — three decimal places on a
number reported as a percentage. Ten million would give ±0.0003 at a hundred
times the memory, and the reported `sample_size` says which was used.

`iterations × outputs` is bounded at 24 million, which is the bound that stops a
request from choosing how much memory a worker uses.

---

## 9. Where a run happens

Section 3 asks the system to decide between local and server-side execution by
size. It does, and the response says which way it went.

```
weighted = iterations × outputs
weighted ≤ 150 000  →  inline, in the request, on a blocking thread
otherwise           →  a durable job on Postgres
```

A round trip through a queue for eighty milliseconds of arithmetic is latency
with nothing to show for it. A two-minute computation inside an HTTP request
dies with the first rolling deploy.

**An inline run still does not block the runtime.** It goes through
`spawn_blocking`: the engine is a tight arithmetic loop with no `await` in it,
and run directly on a Tokio worker thread it would stall every other request
that thread was multiplexing.

**A queued run lives in the analysis worker**, not a fourth process. An analysis
and a simulation are the same shape of workload — minutes of CPU, nobody holding
a socket, a result written at the end — so they want the same node pool, memory
limits and disruption budget. A notification is different in every one of those
respects, which is why *that* has a Deployment of its own.

Analyses are claimed first. Under saturation the work someone is watching a
progress bar for should win.

### Progress and cancellation

The engine knows only an atomic flag; cancellation lives in a database row. A
watcher task bridges them: every two seconds it writes the completed iteration
count back — which is what a browser's progress bar reads — and flips the flag
when a cancellation arrives. The engine checks it once per 4 096-iteration
batch, because at a million iterations an atomic load in the inner loop costs
more than the arithmetic it guards.

A cancelled run stores **no statistics**. A partial result from a sample whose
size the reader has no way to know is worse than no result.

---

## 10. Security

Everything under `/v1/simulations` sits behind the same authorisation as the
rest of the API, with two new permissions.

| | Owner | Member | Advisor |
|---|---|---|---|
| `RunSimulation` | ✓ | ✓ | ✓ |
| `ReadSimulation` | ✓ | ✓ | ✓ |
| `ReadAuditTrail` | ✓ | — | — |

An advisor may run scenarios: it is the work they were engaged for, it touches
no document they cannot already read, and it changes nothing. They cannot read
who else ran what.

All nine tables carry `company_id` and have row-level security **forced**. None
of them is a queue — the worker learns which company it is acting for from the
job row before touching any of them — so there is no cross-tenant scan to
accommodate and nothing is weakened. A simulation belonging to another tenant
answers **404, not 403**: whether it exists is not the caller's business.

Rate limiting has its own bucket, at 120 runs an hour rather than the analysis
bucket's 20. A simulation costs CPU and no model tokens, and it must not be
possible to exhaust the analysis quota by running scenarios. The real ceiling on
cost is the engine's iteration bound, not the counter.

---

## 11. The audit trail

Every event carries the actor, and for a session that is the person rather than
"api".

| Event | Records |
|---|---|
| `simulation.created` | Name, input and output counts, model hash |
| `simulation.version_added` | Version number, new hash, note |
| `simulation.run_requested` | Run id, **seed**, iterations, model version, engine version, execution mode, reason |
| `simulation.run_completed` | Duration, iterations, how many outputs failed to converge |
| `simulation.run_cancelled` | Who, and how far it had got |

WHO, WHAT, WHEN, WHY, WITH WHICH INPUTS, WITH WHICH VERSION, WITH WHICH SEED,
WITH WHICH MODEL, RESULT — all present, and enough to re-run any historical
result and get the same numbers.

---

## 12. The interface

`/simulations`, sharing the design system in `ui/app.css` with the main
interface — extracted there when the second page arrived, because duplicated
design tokens are the ones that drift apart one release at a time.

Charts are hand-drawn SVG. No charting library: the page loads nothing from
anywhere else, which is what makes the content security policy trivial, and the
four charts here need axes, bars, a polyline and dashed markers.

- **Histogram** with a density curve, and dashed markers at P10, P50, P90, the
  mean, the target and the critical threshold.
- **Cumulative probability**, which reads as "there is an X% chance the result is
  at least Y" — the sentence the specification asks for, rendered as a sentence.
- **Tornado**, ranking inputs by their share of the variance.
- **Convergence**, as a table: four series over five checkpoints is a table, and
  drawing it as a chart would be decoration.

**Editing any parameter marks the result stale** — a banner appears, the button
changes to "Kör om simuleringen", and the numbers on screen are labelled as
belonging to the previous inputs. A screen that silently keeps showing old
numbers beside new inputs is the failure mode this whole layer exists to avoid.

Accessibility: every chart has an `aria-label` carrying its headline numbers,
the histogram is keyboard-navigable bin by bin with the same readout the mouse
produces, and there is a `<details>` table of the same data underneath. A chart
reachable only with a mouse is a chart half the people who need it cannot read.

Editing a parameter creates a **new model version** rather than mutating the
current one, so the run history stays honest about what produced what.

---

## 13. Hardening

Written after the layer was built, from an attempt to break it rather than from
a checklist.

### The denial of service that every other limit missed

`Discrete` and `Custom` are the only distributions whose **cost per draw is
chosen by the request** rather than fixed by the mathematics: both were sampled
by scanning their outcomes. A 12.6 MB request body — well inside the API's
32 MB limit — carried a million outcomes, and a 50 000-iteration run over it
took **55 seconds**. That run is small enough to be answered inside the HTTP
request, so it held a blocking thread for a minute while passing the iteration
bound, the memory bound, the rate limit and the body limit.

Three changes, because the class needed closing from both ends:

| | |
|---|---|
| **The input** | `MAX_CATEGORIES = 1 000`. A scenario variable with more than a thousand named outcomes is not a scenario variable |
| **The draw** | A cumulative table built once per run and searched by bisection. At the bound that is ten comparisons instead of a thousand additions — measured at 38 ms for a million draws, against roughly ten seconds before |
| **The decision** | `execution_for` now weighs the model's **cost per iteration** — inputs plus expression node counts — not its output count. A two-output model with a branch in every expression can cost more per iteration than a ten-output model of products, and the cheap-looking one was being answered inline |

The bisection changed one input value per outcome boundary: the scan gave
`u == edge` to the outcome below it, bisection gives it to the outcome above,
which is the standard half-open convention and the correct one. One value out of
2^53 per draw, so no statistic moves — and `ENGINE_VERSION` was bumped to 1.1.0
for it anyway. A rule that a change to what a seed produces bumps the version is
only worth having if it is applied when the difference is negligible.

### Two backstops behind the estimate

The cost model decides from the model's shape, which is an estimate.

- **A five-second deadline** on any run answered inside a request. Exceeding it
  abandons the run with a 503 that says to raise the iteration count so it
  queues. No request can hold a blocking thread for longer, however the
  arithmetic surprises us.
- **Four concurrent inline runs**, process-wide. A rate limit is per tenant and
  per hour; this is the guard a rate limit cannot be. Without it, enough tenants
  running at once consume the Tokio blocking pool and every request that needs
  it — including the readiness probe — stops being served.

### The other bounds

Every text field is bounded: 200 characters for a name or unit, 2 000 for a
description or source, 4 000 for an expression. None of them had a length of
its own before — only the request body did, so a single 30 MB description would
have been accepted, stored in a JSONB column and rendered into a page. Lengths
are counted in characters rather than bytes, so a Swedish description is not cut
short for containing `ä`.

Creating a model and adding a version were the two endpoints with **no rate
limit at all**: both write rows and neither costs enough per call to be
self-limiting. They now share the run bucket.

### The browser side

The API serves two HTML pages and set **no security headers whatsoever**. It now
sends a policy with no exception in it:

```
default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:;
font-src 'self'; connect-src 'self'; form-action 'none'; frame-ancestors 'none';
base-uri 'none'; object-src 'none'
```

`script-src 'self'` with no `'unsafe-inline'` is the directive that stops an
injected `<script>` from executing, and it was unreachable while both pages
carried their script inline — an injected script is inline too, so allowing
inline would have allowed the attack. The scripts moved into files. Fourteen
inline `style="…"` attributes became four utility classes, and the one length
that genuinely comes from data — a bar width in the tornado chart — is set
through the CSSOM, which the policy does not govern.

Verified by doing rather than by reading: the browser suite injects a
`<script>` into the live page and asserts it does not run. It also caught the
policy working against the test harness itself, since Playwright's
`waitForFunction` evaluates a string.

Non-page responses get `default-src 'none'; sandbox` — a browser navigated
directly at an endpoint renders whatever it is given — plus `nosniff`,
`no-referrer`, `frame-ancestors`, `Permissions-Policy` with the device APIs
switched off, cross-origin isolation, and a year of HSTS.

**One thing this pass corrected in itself.** HSTS was first gated on the
development switch that disables `Secure` cookies. That was wrong twice over:
RFC 6797 requires a browser to ignore the header over a non-secure transport,
so it pins nothing on a loopback server — and in production the API speaks plain
HTTP to a TLS-terminating ingress, so a condition on its own transport answers
the wrong question and could leave the header off where it matters. It is sent
unconditionally.

### What extracting the scripts exposed

The disclaimer on the main page was set from a constant **in the page's own
JavaScript**. A test asserted it was in the served HTML and passed only because
the script was inline. Moving the script into a file broke the test and revealed
the real problem: a page whose script failed to load showed an empty paragraph
where a required disclaimer belongs. The text is now in the markup, and the
script only replaces it with the server's wording once a result arrives.

---

## 14. What is not built

- **Correlated inputs.** Every input is independent. Correlated inputs would
  need a copula or a Cholesky factor on a correlation matrix, and — more
  importantly — would invalidate the variance decomposition in §7, which is
  stated as an assumption rather than hidden. Not implemented, because
  implementing it speculatively would ship a sensitivity analysis that quietly
  means something different.
- **Discrete and custom distributions are not editable in the web interface.**
  They are fully supported by the engine, the API and the contract; the form
  only renders scalar parameters. A client can send them today.
- **Optimisation and goal-seek.** "What input value would make P50 reach the
  target" is a different tool and is not here.
- **Time series.** Every run is one period. A multi-period model would need
  state between iterations, which the expression language deliberately cannot
  express.
