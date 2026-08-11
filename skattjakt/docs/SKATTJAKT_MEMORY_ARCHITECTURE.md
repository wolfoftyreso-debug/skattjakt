# Skattjakt — Memory architecture

The four layers §11 requires, each with an owner, a source of truth, a schema, a
retention, an access control, a backup and a deletion path.

The section's closing rule is the one worth putting first, because it is the
mistake this document exists to prevent:

> **Cache får aldrig förväxlas med permanent memory.**

Every layer below states which side of that line it is on. A layer whose loss is
survivable and a layer whose loss is a customer's data are different things and
must never be stored in the same place with the same expectations.

---

## 0. The layers at a glance

| Layer | Lives in | Loss means | Retention |
|---|---|---|---|
| **Short-term state** | Process memory, one request or one job | Nothing; it is rebuilt | Seconds to minutes |
| **Long-term state** | PostgreSQL + object storage | Customer data is gone | 730 days, per tenant |
| **System memory** | PostgreSQL, append-only | We cannot say what happened | 3650 days |
| **Model/context memory** | Assembled per call, stored as a record | Reproducibility is lost | With the analysis |

---

## 1. Short-term state

**Owner:** the process handling the request or the job.
**Source of truth:** nothing. It is derived, and it is *always* rebuildable.

| What | Where | Lifetime |
|---|---|---|
| Request context — correlation id, trace context, tenant | A `Tenant<'_>` transaction | One request |
| Extraction working state — pages, candidate facts | The worker's stack | One job |
| Rate-limit read | A row read per request | One check |
| Rendered report | Computed on read | One response |

**There is no cache tier.** No Redis, no in-process memoisation of anything a
customer can see. That is a decision, and §33's questions answer it: a cache
would need a second stateful system to secure, back up, monitor and upgrade, in
exchange for saving milliseconds on requests that are already fast. The day a
profile shows a hot read path, a cache goes in with an explicit invalidation
story — not before.

**Rate limiting is deliberately not in memory.** Several API replicas serve one
customer, and an in-process limiter multiplies every quota by the replica count,
which is the same as not having one. It is a database row, and paying for that
read is the point.

**Nothing here is backed up**, because nothing here is a source of truth. If a
pod dies mid-analysis, every byte of this layer is lost and the analysis is
retried from the job row — see `SKATTJAKT_ARCHITECTURE.md` §4.

---

## 2. Long-term state

**Owner:** PostgreSQL for structured data, object storage for blobs.
**Source of truth:** yes, for everything a customer would say is theirs.

| What | Table | Notes |
|---|---|---|
| Companies and profiles | `companies` | `profile` is JSONB: a questionnaire that will grow |
| People and membership | `users`, `company_members` | A person belongs to several companies |
| Documents | `documents`, `document_versions` | Versions immutable, content-addressed |
| Document bytes | Object storage | Key derived from identifiers, hash verified on read |
| Extracted facts | `financial_facts` | **Every reading kept**, not just the winner |
| Analyses and results | `analysis_jobs`, `opportunities`, `opportunity_evidence`, `calculations` | |
| Credentials | `user_credentials` | Argon2id; never reversible |
| Sessions and devices | `sessions`, `devices` | Tokens as SHA-256 only |

**Access control:** PostgreSQL row-level security with `FORCE`, so a query that
forgets its tenant returns nothing rather than everything. Detail in
`SKATTJAKT_SECURITY.md` §2.

**Retention:** 730 days by default, per tenant, in `retention_policies`.

**Backup:** daily `pg_dump`, encrypted with `age`, stored off-cluster — and a
**weekly restore test** that checks the tables came back, the row counts are
plausible, and forced row-level security survived the round trip. A restore that
produces a structurally correct but empty database passes the first two checks
and fails the third, which is the case a naive test misses.

**Deletion:** recorded in `deletion_requests` *before* anything is removed, with
separate timestamps for the database and the blobs. Blobs first, because the
storage key is reachable only through the row. A deletion that half-completed
and left no record of itself is the one failure that cannot be recovered from,
because nobody knows what is missing.

---

## 3. System memory

**Owner:** PostgreSQL.
**Source of truth:** yes — and uniquely, it is the source of truth for what the
*system* did, which nothing else records.

| What | Table | Append-only |
|---|---|---|
| Audit events | `audit_events` | ✓ `UPDATE`/`DELETE` revoked |
| Job transitions | `job_transitions` | ✓ |
| Dead letters | `dead_letters` | ✓ `DELETE` revoked; acknowledged, never removed |
| Model runs | `model_runs` | Which model, what it cost, whether it was a fallback |
| Rule set approvals | `rule_set_approvals` | ✓ frozen once decided |
| Rule versions in force | `rule_versions` | |

Append-only is enforced by the grant, not by convention:

```sql
REVOKE UPDATE, DELETE ON audit_events FROM skattjakt_app;
```

A history that can be rewritten is not a history, and a rule approval that can
be edited afterwards is not a decision.

**Retention is 3650 days — deliberately ten times the data it describes.** That
looks inconsistent until you ask what it holds: identifiers, outcomes and
timings, not the customer's economy. It is also the only record of *what was
deleted and when*, which is precisely what a deletion request has to be able to
demonstrate afterwards. Deleting the evidence of a deletion along with the data
would make the deletion unprovable.

---

## 4. Model and context memory

**Owner:** `skattjakt-gateway` assembles it; `model_runs` records it.
**Source of truth:** for *what was sent and what came back* — yes. For what is
true about the company — never.

This is the layer where §11 asks about embeddings and vector representations,
and the honest answer is a decision rather than an omission.

### What context a model call carries

Assembled per call, from data already in the database:

1. The canonical financial facts for this analysis.
2. The company profile.
3. A bounded excerpt of the document text, wrapped as data inside a verified
   fence.
4. For the falsification pass: the candidates from the discovery pass.

Nothing else. In particular, no history of previous analyses, no other
customer's data, and no accumulated "what we know about this company" store.

### There is no vector store, and that is the design

§11 lists "embeddings/vector representations där det faktiskt behövs", and
§33 asks why any technology is present. For Skattjakt the answer is that the
need does not arise:

- **The corpus is one company's accounts**, tens of pages, and it fits in a
  context window whole. Retrieval solves the problem of choosing what to send
  when you cannot send everything; here you can.
- **The rules are structured data, not prose.** They are evaluated by a rule
  engine against typed facts. Retrieving a rule by semantic similarity would be
  replacing an exact, auditable, citable lookup with an approximate one — in a
  product whose entire claim is that every finding is traceable to a cited rule.
- **A similarity match cannot be cited.** "This is like something else" is not
  something a customer can take to their accountant.

A vector store would add a second stateful system to back up, secure and
operate, for a retrieval problem that does not exist. If it ever does — a corpus
of Skatteverket guidance too large to send — it would be an *additional* input
to the discovery pass, and it would still not be allowed to satisfy the evidence
gate. Retrieval could propose; only a versioned rule and a document value can
support.

### Cross-analysis memory is deliberately absent

Skattjakt does not remember what it concluded about a company last year, and
does not feed it into this year's analysis.

That would be an obvious feature and it is the wrong one here. A finding that
rests partly on last year's conclusion is a finding whose evidence chain leaves
the documents in front of it — and the rule that nothing is actionable without a
value read from a specific page of a specific document version is the property
that makes the output trustworthy. Compounding a mistake across years is exactly
the failure a tax product must not have.

What *is* kept across analyses, and is enough: the documents, the facts, the
rule set version, and the audit trail. A human comparing two years has all of
it. The system simply does not use last year's conclusion as this year's input.

### No chain-of-thought is stored

`model_runs` records the structured conclusion, the token counts, the latency,
the model that served it and the cost. Section 21 permits conclusions, evidence,
calculations, a rationale summary and validation state; reasoning traces are not
on that list, and the response type has nowhere to put one. A test asserts the
serialised shape.

**Retention:** with the analysis, 730 days. **Deletion:** cascades from the
company. **Access control:** `model_runs` is a tenant table under row-level
security.

---

## 5. The classification each layer may hold

| Layer | Highest classification |
|---|---|
| Short-term state | `HIGHLY_CONFIDENTIAL` — it holds the figures, in memory, briefly |
| Long-term state | `HIGHLY_CONFIDENTIAL` |
| System memory | `CONFIDENTIAL` — identifiers and outcomes, never amounts |
| Model context | `HIGHLY_CONFIDENTIAL` — the only layer that leaves the cluster |
| Logs / metrics / traces | `INTERNAL` / `PUBLIC`, enforced at the emitter |

System memory being capped at `CONFIDENTIAL` is what makes its ten-year
retention acceptable. If `job_transitions` could carry an amount, keeping it for
a decade would mean keeping the customer's economy for a decade after they asked
for it to be deleted.

The model request boundary is the only place `HIGHLY_CONFIDENTIAL` leaves the
cluster, and it does so at one named constant, because sending the figures is
what the customer asked for.

---

## 6. What is deliberately absent, in one list

Per §33 — every technology must justify itself, and so must every one that is
missing:

| Not present | Why |
|---|---|
| Redis / memcached | No hot read path measured. Would add a stateful system to back up and secure for milliseconds |
| A vector store | The corpus fits in a context window; rules are structured and must be citable |
| An event store / event sourcing | The audit trail and `job_transitions` answer the questions event sourcing is reached for, without rebuilding state from a log |
| A graph database | The evidence graph is derived in memory from data already in Postgres, hundreds of nodes per analysis |
| A search engine | Nobody searches across analyses yet. When they do, Postgres full-text is the first answer, not the fallback |
| Cross-analysis model memory | Would let a finding rest on last year's conclusion instead of this year's documents |

The shape of the argument is the same each time: **fewer data technologies,
enough to model the problem** (§10), and the burden is on adding one.
