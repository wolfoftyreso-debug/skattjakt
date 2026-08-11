# Skattjakt — Data model

22 tables across three migrations. This document says what each holds and, where
the shape is not obvious, why it is shaped that way.

The schema is the source of truth. `migrations/*.sql` carries the same reasoning
inline; this is the map.

---

## 1. The shape

```
companies ─┬─ company_members ── users
           │
           ├─ documents ── document_versions ─┬─ financial_facts
           │                                  │
           ├─ analysis_jobs ──────────────────┘
           │       │
           │       ├─ model_runs
           │       ├─ analysis_budgets
           │       ├─ calculations
           │       └─ opportunities ── opportunity_evidence
           │
           ├─ jobs ── job_transitions
           │     └─ dead_letters
           │
           ├─ api_tokens
           ├─ rate_limit_counters
           ├─ retention_policies
           ├─ deletion_requests
           └─ audit_events

rule_versions          (global)
rule_set_approvals     (global)
```

---

## 2. Tenancy

`companies.id` is the tenant. Every tenant table carries a `company_id` and
lives under a row-level security policy:

```sql
ALTER TABLE t ENABLE ROW LEVEL SECURITY;
ALTER TABLE t FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON t
    USING (company_id = current_company_id())
    WITH CHECK (company_id = current_company_id());
```

`companies` itself keys on its own `id` rather than on a `company_id` column,
and has its own policy accordingly.

**Four tables sit outside RLS, each deliberately:**

| Table | Why | What bounds the risk |
|---|---|---|
| `api_tokens` | Authentication precedes knowing the tenant | Only SHA-256 stored |
| `jobs` | A queue is scanned across tenants by definition | Identifiers, state, timing — no payload column at all |
| `job_transitions` | Belongs to `jobs` | Same, plus append-only |
| `dead_letters` | Belongs to `jobs` | Same |

The alternative for `jobs` — RLS plus a `BYPASSRLS` worker role — is strictly
worse: that role would also bypass isolation on the tables holding the
customer's economy. The chosen trade keeps the bypass confined to a table that
holds nothing worth bypassing for.

`current_company_id()` returns `NULL` when the setting is unset, so every policy
matches nothing. **A query that forgets its tenant returns zero rows.**

---

## 3. Core tables

### `companies`

The tenant. `org_number` is stored as ten digits, Luhn-validated on the way in.
`profile` is `JSONB` for the optional attributes (industry, employee count,
ownership) because they are a questionnaire that will grow, and a column per
question would mean a migration per question.

The fiscal year is two dates rather than a year integer: a Swedish AB may have a
broken fiscal year, and an analysis that assumed a calendar year would silently
apply the wrong year's rules.

### `documents` / `document_versions`

Separate because a document is re-uploaded and an analysis must remain
reproducible.

`document_versions` is immutable. Each row carries `sha256`, `byte_size`,
`mime_type`, `page_count` and a `storage_key` of the form:

```
companies/{company_id}/documents/{document_id}/v{n}-{sha256}
```

Derived entirely from identifiers, never from a filename — the structural answer
to path traversal. The hash is verified on every read, so a blob that no longer
matches what was recorded cannot quietly become the basis of an analysis. That
is the difference between a storage fault and a wrong tax answer.

### `financial_facts`

One row per figure read from a page, with its `document_version_id`, `page`,
`source_text` and an extraction confidence.

**Every reading is kept, not just the winner.** Two conflicting readings of
revenue are a contradiction the analysis must report, and a schema that stored
only the best guess would have thrown away the evidence for it. The canonical
value is the highest-confidence reading, computed on read.

Costs are stored as positive magnitudes even though Swedish statements print
them negative. `source_text` keeps the printed sign. Without this normalisation
every rule touching a cost would need its own `abs()`, and the one that forgot
would fail silently — which is exactly the bug the golden dataset caught.

### `analysis_jobs`

`document_version_ids UUID[]` pins the exact versions at creation, so a later
upload cannot change what a finished analysis was based on.

`rule_set_version` is stored alongside, so an old analysis can be explained
against the rules that were in force when it ran, not against today's.

`accounts_state` records whether the run read preliminary or final accounts. It
was previously carried only in the request that started the run, which made the
run impossible to reproduce once a worker rather than the request did the work.

### `model_runs`

What was asked of a model and what came back: provider, model, task, prompt
version, token counts, latency, and the **structured conclusion**.

Never the reasoning trace. Section 21 permits conclusions, evidence,
calculations, a rationale summary and validation state; chain-of-thought is not
on that list and there is no column for it.

Migration 0003 adds `requested_model`, `served_by_model`, `was_fallback` and
`cost_micro_ore` — so an analysis produced by a model nobody chose is a fact on
the record rather than an invisible substitution.

### `opportunities` / `opportunity_evidence`

A finding, and its chain.

Money is two `BIGINT` columns in öre, `impact_low` and `impact_high`. There is
no single-amount column, in the database or in the domain model. A point
estimate is not representable.

`opportunity_evidence` is one row per link — a document value, a rule, a
calculation, a model judgement, an assumption — with `position` preserving the
order. `status` and `rejection_reason` keep findings the falsification pass
removed, because "we considered this and rejected it" is a result.

### `calculations`

Method, inputs and result for every deterministic computation, so a figure can
be re-derived rather than trusted. Section 6: calculations are code, never a
model's arithmetic.

### `audit_events`

Append-only for the application:

```sql
REVOKE UPDATE, DELETE ON audit_events FROM skattjakt_app;
```

A history that can be rewritten is not a history.

---

## 4. The job system (migration 0003)

### `jobs`

Two check constraints are load-bearing, and they encode invariants the Rust code
also maintains — belt and braces, because the database is the last line:

```sql
CONSTRAINT lease_is_whole CHECK (
    (leased_until IS NULL AND leased_by IS NULL)
    OR (leased_until IS NOT NULL AND leased_by IS NOT NULL)
),
CONSTRAINT lease_only_while_running CHECK (
    state = 'running' OR leased_until IS NULL
)
```

A lease without a holder is a job that can never be reaped: the reaper looks for
an expired `leased_until` and the row would never name a pod to blame. The
database refuses to store one. `tests/failure/job-failures.sh` proves both.

The idempotency index is scoped per tenant:

```sql
CREATE UNIQUE INDEX jobs_idempotency ON jobs (company_id, kind, idempotency_key);
```

Per tenant so one customer's key cannot collide with — or probe for — another's.

**There is no `payload` column, deliberately.** A job carries a `subject_id`;
the worker reads the subject through a tenant-scoped transaction. That keeps the
only path to customer data running through RLS, which is what makes it
acceptable for this table to sit outside it.

### `job_transitions`

Every state change, written in the same transaction as the change itself, so the
history cannot disagree with the current value. Append-only.

`detail` holds a failure *kind* — `provider_timeout`, `pdf_unreadable` — never a
message. Anything read out of a customer's document would end up in an
operator's queue view.

### `dead_letters`

A separate table rather than a state flag, because a dead-lettered job needs
things a job row does not have: who acknowledged it, when, and what they
decided. `DELETE` is revoked — a dead letter is acknowledged, never removed.

---

## 5. Cost, limits and retention

### `analysis_budgets`

`limit_micro_ore`, `spent_micro_ore`, `calls`, `exceeded_at`.

Costs are integers in **micro-öre**. Öre alone is too coarse: a cheap call costs
a fraction of one, would round to zero, and a thousand of them would cost
nothing at all — the exact shape of a bill nobody noticed accruing. The same
objection to floating point that governs `Money` governs this.

Per analysis rather than aggregated on read, so the budget check before each
model call is a single row read.

### `rate_limit_counters`

A fixed-window counter keyed `(company_id, bucket, window_start)`.

In the database rather than in process memory because the API runs several
replicas, and an in-memory limiter would multiply every quota by the replica
count — which is the same as not having one.

A fixed window lets through up to twice the quota across a boundary. That
weakness is accepted: the quotas exist to stop runaway clients and cost
blowouts, not to shape traffic to the request, and a token bucket would need a
row lock per request on the hot read path.

### `retention_policies` / `deletion_requests`

Per-tenant retention, defaulting to 730 days for documents and analyses and
3650 for audit events.

The audit trail outlives the data it describes on purpose: it holds identifiers
and outcomes, not the customer's economy, and it is the only record of what was
deleted and when — which is exactly what a deletion request must demonstrate
afterwards.

`deletion_requests` is written **before** anything is removed, with separate
`db_done_at` and `blobs_done_at` timestamps. A deletion that half-completed and
left no record of itself is the one failure mode that cannot be recovered from,
because nobody knows what is missing.

Deletion order matters and is encoded in `purge_expired_analyses`: children
before parents, because the foreign keys exist to prevent exactly the orphan a
careless order would create. Facts derived from a document version go with it —
section 65 covers derived data, and an extracted fact holds the same figure the
document did.

---

## 6. Rule governance

### `rule_versions`

Which rule set was in force, so an old analysis can be explained against it.

### `rule_set_approvals`

The workflow of section 53, enforced by the database rather than by process:

```sql
CONSTRAINT reviewer_is_not_the_proposer CHECK (
    reviewed_by IS NULL OR reviewed_by <> proposed_by
),
CONSTRAINT a_decision_names_its_reviewer CHECK (
    approved IS NULL OR (reviewed_by IS NOT NULL AND reviewed_at IS NOT NULL)
)
```

A workflow enforced only by process is a workflow that gets skipped at 17:55 on
a Friday. `UPDATE` and `DELETE` are revoked once a decision is recorded: a
changed mind is a new version, not an edit.

`affected_analyses` is filled from the evidence graph before approval, so a rule
change states its blast radius before anyone can approve it.

`tests/failure/job-failures.sh` proves all three properties.

---

## 7. Conventions

**Money.** `BIGINT` öre. Costs in `BIGINT` micro-öre. Never `NUMERIC`, never
floating point, never a single amount where a range is meant.

**Time.** `TIMESTAMPTZ`, always UTC. A fiscal year is two dates.

**Identifiers.** `UUID` v4, typed in Rust so a `DocumentId` cannot be passed
where a `CompanyId` is expected — that substitution is exactly how cross-tenant
leaks are written, so the compiler is made to care.

**Enumerations.** `TEXT` with a `CHECK` constraint rather than a Postgres enum
type. Adding a value is a migration either way; a `CHECK` can be altered without
a table rewrite, and the value is readable in a `pg_dump` without a type
lookup.

**Soft delete.** None. Retention deletes rows; the audit trail records that it
happened. A `deleted_at` column is a leak waiting for the one query that forgets
to filter on it.

**Cascades.** `ON DELETE CASCADE` from `companies` downwards, so deleting a
tenant genuinely removes the tenant. Blobs are deleted first, by
`expired_document_versions` then `purge_document_versions`, because the storage
key is reachable only through the row.

---

## 8. Migrations

| File | Contents |
|---|---|
| `0001_init.sql` | 13 tables, RLS, the `skattjakt_app` role, grants, `current_company_id()` |
| `0002_api_tokens.sql` | Token authentication; outside RLS, documented |
| `0003_jobs_and_governance.sql` | Queue, transitions, dead letters, budgets, rate limits, retention, deletion, rule approvals |

Applied by the owning role. The application role deliberately cannot create
tables, so a compromised application cannot alter its own schema.

Migrations are forward-only. There are no `down` scripts: a rollback that drops
a column drops the data in it, and the recovery path for a bad migration is a
restore, which is tested weekly.
