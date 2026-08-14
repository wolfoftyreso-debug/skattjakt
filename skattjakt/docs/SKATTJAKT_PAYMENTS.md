# Skattjakt — Payments

Taking 29 or 69 kronor for one analysis, and refusing to run one nobody paid
for.

---

## 1. The position everything else follows from

> **A payment is settled by asking Swish, never by being told.**

Swish delivers a callback when a payment resolves. This system reads one field
from it — the payment reference — and discards the rest. The outcome comes from
a fresh `GET` to Swish over the same mutually-authenticated connection the
payment was created on, and *that* answer is what moves an order.

The consequence is worth stating plainly, because it is the reason the callback
endpoint needs no authentication:

| An attacker who… | achieves |
|---|---|
| Posts a forged callback naming a real order | one outbound lookup; Swish says unpaid; nothing changes |
| Replays a real callback | the same lookup twice; settlement is idempotent |
| Guesses a payment reference | the ability to make us ask Swish a question |

Callback authentication is a common place to lose money, because getting it
subtly wrong is easy and the failure is silent. The strongest defence available
is not to depend on it.

`tests/integration/payments.sh` asserts this against a running API: a forged
callback carrying a real order's reference and `"status":"PAID"` leaves the
order exactly where it was.

---

## 2. What the bank supplies

Swish Handel is arranged through the bank — Nordea, in this case — not through
Swish directly. Three things come out of it, and only the first is a decision:

| What | Where it comes from | Where it goes |
|---|---|---|
| **Swish-nummer** (`123XXXXXXX`) | Assigned by the bank when the agreement is signed | `SKATTJAKT_SWISH_PAYEE_ALIAS` |
| **Client certificate** — a private key and a certificate signed via the bank | Generated in Swish Certificate Management; the CSR is made by you, the signing by them | Mounted at `/etc/skattjakt/swish/client.pem` |
| **Swish's CA certificate** | Published by Swish | Mounted at `/etc/skattjakt/swish/swish-ca.pem` |

The certificate is the whole of the secret material. There is no API key and no
shared secret — the mutual TLS handshake *is* the authentication.

### Making the key, and where it must not go

Generate the key where it will live, and never let it leave:

```bash
openssl req -new -newkey rsa:2048 -nodes \
  -keyout swish-private.key -out swish.csr \
  -subj "/C=SE/O=<company>/CN=<company>"
```

Upload `swish.csr` in the bank's certificate management. What comes back is a
signed certificate; concatenate it with the private key into the PEM this
system reads:

```bash
cat swish-signed.pem swish-private.key > client.pem
```

Then, and only then, into Kubernetes:

```bash
kubectl -n skattjakt create secret generic skattjakt-swish \
  --from-file=client.pem=client.pem \
  --from-file=swish-ca.pem=swish-ca.pem
```

**Neither file goes in git.** The repository holds no placeholder for them
either: a committed placeholder is worse than none, because it is the thing
somebody eventually fills in and commits.

---

## 3. Configuration

| Variable | Meaning |
|---|---|
| `SKATTJAKT_SWISH_PAYEE_ALIAS` | The Swish number. **Empty means payments are off** |
| `SKATTJAKT_SWISH_BASE_URL` | Defaults to the *test* host (MSS) |
| `SKATTJAKT_SWISH_CLIENT_PEM` | Path to the certificate and key |
| `SKATTJAKT_SWISH_CA_PEM` | Path to Swish's CA |
| `SKATTJAKT_SWISH_CALLBACK_URL` | Public HTTPS URL of `/v1/payments/swish/callback` |
| `SKATTJAKT_PAYMENTS_REQUIRED` | Whether an analysis needs a paid order |

And the seller's own details, which the shop pages in section 10 are built from:

| Variable | Meaning |
|---|---|
| `SKATTJAKT_MERCHANT_NAME` | Registered company name. **Empty means the shop pages are unconfigured** |
| `SKATTJAKT_MERCHANT_ORG_NUMBER` | Organisationsnummer |
| `SKATTJAKT_MERCHANT_ADDRESS` | Postal address |
| `SKATTJAKT_MERCHANT_EMAIL` | Where a customer writes about an order |
| `SKATTJAKT_MERCHANT_PHONE` | Optional; the only optional one |
| `SKATTJAKT_MERCHANT_VAT_REGISTERED` | Whether VAT may be stated on a price |

Setting the name commits to the rest: the other three required fields are then
refusals to start, naming the one that is missing. A business below the VAT
registration threshold must not state VAT on a price, so
`SKATTJAKT_MERCHANT_VAT_REGISTERED` changes what the price page is permitted to
say rather than only what it shows.

Six things are refusals to start rather than warnings, because each one
produces a deployment that takes orders it cannot collect on — or takes them
without saying who is collecting:

- a Swish number that is not `123` + seven digits;
- `SKATTJAKT_PAYMENTS_REQUIRED` with no provider configured;
- a certificate file that cannot be read or parsed;
- a callback URL that is not `https://`;
- payments required with no merchant configured;
- a merchant name set without its organisationsnummer, address or email.

The default base URL is the **test** environment. Getting that wrong in this
direction fails a TLS handshake; the other way round moves real money.

### Going live

1. Test host, `SKATTJAKT_PAYMENTS_REQUIRED=0`. Orders can be created and paid
   with the Swish merchant simulator; analyses still run without one.
2. Test host, `SKATTJAKT_PAYMENTS_REQUIRED=1`. The gate is live; every analysis
   needs an order. Verify a full purchase.
3. Production host, `SKATTJAKT_PAYMENTS_REQUIRED=1`.

Step 2 is the one people skip, and it is the one that catches a callback URL
that is not reachable from the internet.

---

## 4. The flow

```
  client                      API                    Swish
    │  POST /v1/orders         │                       │
    ├─────────────────────────►│                       │
    │                          │ order + payment row   │
    │                          │ written first         │
    │                          ├──── PUT payment ─────►│
    │◄──── 201 + token ────────┤◄─── 201 + token ──────┤
    │                          │                       │
    │  app switch / QR         │                       │
    ├──────────── payer approves in the Swish app ────►│
    │                          │◄─── callback ─────────┤   a hint
    │                          ├──── GET payment ─────►│   the truth
    │                          │◄──── PAID ────────────┤
    │  GET /v1/orders/{id}     │                       │
    ├─────────────────────────►│  state: paid          │
    │                          │                       │
    │  POST /v1/analyses/stored with order_id          │
    ├─────────────────────────►│  order → consumed     │
```

The client polls `GET /v1/orders/{id}`, which reads the order and does not call
Swish. Polling faster does not drive traffic at the payment provider.

### When the callback never arrives

It happens: a deploy during the thirty seconds a payer spends in the app, a
partition, a bad minute at either end. The analysis worker sweeps every sixty
seconds for payments unresolved for more than thirty, and asks Swish about each.

That sweep is what turns the callback from a requirement into an optimisation.
With it, settlement is fast; without it, settlement still happens. It is also
the only thing that resolves a payment the payer simply abandoned, because an
abandoned payment produces no callback at all.

---

## 5. What is checked before an order is accepted as paid

Three things, in `skattjakt_payments::settle`, each a real failure:

1. **The reference matches.** Otherwise a payment for one order could settle
   another — exactly what happens if a callback body is trusted to name its own
   order.
2. **The amount matches**, in either direction. A mismatch means the order and
   the payment disagree about what was bought, and quietly accepting the larger
   one hides that as surely as accepting the smaller.
3. **The currency is SEK.** This should never vary, which is precisely why an
   unexpected value means something is wrong enough to stop.

A payment Swish calls successful and this system refuses raises
`SkattjaktPaymentsRefusedAfterSuccess`, at critical. Money has moved and nothing
was delivered — runbook §15B.

---

## 6. One order, one analysis

Enforced by the database, not by a handler:

```sql
UPDATE orders SET state = 'consumed', analysis_id = $2 …
WHERE id = $1 AND state = 'paid'
RETURNING …
```

The condition and the transition are the same statement, so two requests racing
on one order cannot both observe `paid`. The loser updates nothing. A unique
index on `analysis_id` makes a second attempt a failed transaction rather than a
support ticket.

A check-then-act in the handler passes every sequential test and fails exactly
where it matters — a customer double-tapping on a slow connection.
`tests/integration/payments.sh` fires ten concurrent requests at one paid order
and asserts that all ten name the same analysis and the order was spent once.

The redemption happens **inside the transaction that creates the analysis**, so
an order cannot be spent on an analysis that then fails to be created.

### What a spent order answers

Not `402`. An order that already names an analysis answers with **that
analysis**, because the customer asking a second time is nearly always the
customer whose first request timed out. They have paid; "that order cannot be
used" would be both false and expensive. One order still buys exactly one
analysis — asking twice shows you the one you bought.

### The order is part of the idempotency key

A request that supplies no `Idempotency-Key` gets one derived from the work, so
a retry does not run the analysis twice. That derivation was
`(kind, company, document_version_ids)` — which does not mention the order, and
so a paid analysis over documents that had been analysed before **collapsed onto
the earlier job**. The order was consumed, a new analysis row was written, and
the customer was handed the *earlier* analysis. They paid and received something
else, and nothing in the system recorded that anything was wrong.

The derived key now carries the order when there is one. Every purchase is its
own piece of work; a retry of the same purchase still derives the same key,
which is the case the derivation exists for.

### Nothing is sold that cannot be delivered

The three products are three presentation layers over one engine, and the engine
only has rules for the taxpayers somebody has written rules for. Rules now say
which kind of taxpayer they are about — an aktiebolag or a private individual —
because Swedish tax law for the two is not the same body of rules, and a finding
from one is not merely less relevant to the other, it is wrong.

`RuleSet::covers_audience` answers the question a shop has to ask before it
takes money, and three places ask it: `/priser` lists the product without a
price, `/tjanster` says it is not open, and `POST /v1/orders` refuses with
`503 product_not_available`.

**Privatanalys is in that state today.** It has a price, a page and a Swish
payment message, and the shipped rule set contains no private-individual rules
at all, so a customer paying 29 kronor would receive an empty report with no way
to tell "we found nothing" from "we looked at nothing". It is listed as closed
rather than hidden, because a service the site describes elsewhere and silently
omits from the price list reads as an oversight.

The answer comes from the rule set rather than a constant, so the day somebody
writes the first private rule the product becomes sellable on its own. Two tests
fail that day — one in `crates/rules` and one in `tests/e2e/shopfront.sh` — and
that is how whoever wrote the rule finds out they have opened a shop.

### What was bought is what is served

The gate checked *that* an order was paid. It did not check *what it was for*.
The report chose its presentation layer from a query parameter:

```
GET /v1/analyses/{id}/report?audience=accountant
```

so 29 kronor of Privatanalys bought the 69-kronor Skattjakt Kontroll report, for
anyone who read the API documentation. The money was verified and the
entitlement was not — the same mistake as letting a client declare its own
payment settled, with the client deciding what it had bought.

Redemption now stamps `analysis_jobs.audience` from the order's product, in the
same transaction and by the same function, and the report reads that column
rather than the query string. A parameter that disagrees is a `403`. There is
no ladder: Bolagsanalys and Skattjakt Kontroll cost the same and are different
reports, not more and less of one report.

An analysis created while payments were not required carries `NULL` — nobody
bought it, so nothing constrains it — and a deferred constraint trigger refuses
to commit a consumed order whose analysis was never stamped.

---

## 7. Refunds

Not automated, deliberately. Swish has an API for them and it needs the same
certificate, but a refund is a decision about a customer relationship rather
than a mechanism, and half a refund path is worse than none.

`OrderState::RefundOwed` exists so the system can record that one is owed —
for an analysis paid for and then undeliverable — without pretending it can
make it. The refund itself is made in the merchant portal by a person.

---

## 8. What is deliberately not collected

Swish returns the payer's phone number on a completed payment. It is not stored.

Nothing in the product needs it: the analysis belongs to the order, the order to
the company. The least surprising thing to find in a breach is what was never
collected in the first place.

---

## 9. Current state

| | |
|---|---|
| Provider | Swish Commerce API v2, mutual TLS |
| Verified against a real Swish endpoint | **No.** No merchant agreement exists yet |
| Verified against a stand-in over real mutual TLS | Yes — `tests/integration/swish-wire.sh`, 16 checks |
| Verified against a real database and a real API | Yes — `tests/integration/payments.sh`, 27 checks |
| Settlement logic | 24 unit tests in `crates/payments` |
| The six pages the scheme requires | Published — `tests/e2e/shopfront.sh`, 46 checks |
| Terms reviewed by a lawyer | **No.** Section 10 |

The wire format in `swish.rs` — URLs, field names, status strings — is written
against the documented v2 Commerce API and **must still be checked against the
specification the bank supplies**. It is now at least exercised: `swish-stub.py`
stands in for Swish the way MinIO stands in for S3 in the other suites, over a
real mutual-TLS handshake with a server that requires a client certificate. That
cannot tell you Swish agrees with the documentation. It does assert, on every
run, that the client sends what this repository believes the documentation says.

Two defects surfaced the first time the client was allowed to speak, and neither
could have been found any other way:

* **The callback resolved nothing.** It read `payeePaymentReference` — *our*
  order id, echoed back — and looked it up among `provider_reference`, which
  holds the instruction id. Two different identifiers; the lookup never matched.
* **Settlement could not see its own payment.** `payments` is FORCE RLS keyed on
  `current_company_id()`, and both the callback's lookup and the reconciliation
  sweep run before the company is known. Read directly they compared
  `company_id = NULL` and returned nothing, always — so settlement was
  structurally impossible, not merely slow. Every test had marked orders paid
  with `psql` as a superuser, which bypasses RLS, so the settlement logic was
  well tested and the lookup that reaches it had never run. Fixed by two narrow
  SECURITY DEFINER functions in `0012`, the same shape as `0004`'s; the suite
  now asserts both **as the application role**, because asserting them as a
  superuser passes whether or not the hole is open. It is concentrated in two structs (`Wire`,
`WirePayment`) precisely so that checking it is reading two structs rather than
auditing a client.

Until a payment has been made against the test host with a real certificate, the
honest description of this is: the gate, the state machine, the double-spend
defence and the settlement rules are tested and hold; the conversation with
Swish has never happened.

---

## 10. The six pages the scheme requires

The Swish Handel application has six checkboxes: prices, product and service
information, terms of purchase, contact details, returns policy, returns
information. Ticking them is not a formality — it is the merchant's attestation
to the bank that these exist on the site named in the form. Two of them are
also statutory in their own right: prisinformationslagen (2004:347) requires a
price to be stated so a consumer can read it, and distansavtalslagen (2005:59)
requires the right of cancellation to be given before the purchase, not after.

They are served by the API rather than by a separate site, so that the price a
page publishes and the price the checkout charges come from the same
`Product::price()`:

| Page | Box it answers |
|---|---|
| `/priser` | Prisuppgifter |
| `/tjanster` | Information om produkter och tjänster |
| `/villkor` | Köpavtal |
| `/kontakt` | Kontaktuppgifter |
| `/angerratt` | Information om returpolicy **and** information om returer |

### Why the details are configuration

Three of the required facts — registered name, organisationsnummer, address —
are not knowable from this repository. A placeholder for them would be the worst
available outcome: the pages would look complete and be false in precisely the
way an attestation must not be. So they come from the environment, a deployment
that takes payment without them refuses to start, and a deployment that has not
configured them serves a page saying it is unconfigured rather than a page with
a blank where the seller's name belongs. `tests/e2e/shopfront.sh` asserts both
directions.

### The one thing on these pages that is not verified

The purchase terms and the cancellation text are a serious draft written against
the statutes cited in the source registry. **No lawyer has read them.** That is
stated on the pages themselves, not only here, because the person who needs to
know is the one reading them.

The specific point a lawyer should look at first is the wording itself, in
`skattjakt_payments::CONSENT_WORDING`, and whether it is enough to carry
distansavtalslagen (2005:59) 2 kap. 11 § 11. The mechanism around it is built
and tested — see section 11 — but nobody qualified has read the sentence.

---

## 11. The choice at checkout

A digital service delivered immediately loses its right of cancellation only if
the consumer has **expressly consented** to delivery beginning and has
**acknowledged** that the right is thereby lost — distansavtalslagen (2005:59)
2 kap. 11 § 11. Publishing that on a terms page is not consent. Consent is
something the buyer does, at the moment of buying.

`/villkor` and `/angerratt` promised the buyer two options from the day they
were written — start now and lose the right, or wait out the fourteen days and
keep it — and the checkout offered neither. A purchase term describing a choice
that does not exist is the worst kind of documentation drift, because it is the
kind a customer can rely on to their cost.

```
POST /v1/orders
{ "product": "company_analysis",
  "delivery": "immediate",
  "accepts_loss_of_cancellation_right": true }
```

Two fields rather than one, because the law asks for two things and one field
could only ever record one of them. `delivery: immediate` without the
acknowledgement is refused (`consent_required`) rather than quietly downgraded
to the safe option — a buyer put in a two-week queue they did not ask for would
find out a fortnight later.

**Silence is never consent.** An order that says nothing about delivery keeps
the right to cancel. The column defaults that way, so even a row inserted by
something that has never heard of this can only err towards the buyer.

### What is recorded, and why the wording is versioned

Proving consent means knowing what was consented to. A boolean would record that
a box was ticked next to text nobody can reconstruct: the wording changes, and
every earlier consent silently becomes a claim about words the buyer never saw.
So the order carries `consent_at` and `consent_wording_version`, `/angerratt`
renders the wording from the same constant the order records, and
`tests/e2e/shopfront.sh` asserts the published words verbatim.

Two database constraints keep the record honest in both directions: consent
without the choice, and the choice without consent, are equally a
misrepresentation of what the buyer agreed to.

### The fourteen days are real

`orders.deliverable_from` is part of the condition that consumes an order, not a
check before it:

```sql
WHERE id = $1 AND state = 'paid' AND deliverable_from <= now()
```

for the same reason the double-spend defence is one statement: read-decide-act
is where a fortnight becomes a few milliseconds. A buyer who asks for their
analysis during the period gets `409 order_not_deliverable_yet` **with the
date** — it is the wait they chose, not a failure.

### Exercising the right

`POST /v1/orders/{id}/cancel`, available until the period runs out or the
analysis starts, whichever comes first. It is one conditional statement for the
same reason again: a cancellation and a redemption racing must not both win.

It does **not** move money. This system cannot make refunds, so it moves the
order to `refund_owed` and says a person will pay it. Reporting a refund that
nobody has made would be the one lie a payments document must not contain.

Every way an order can fail to be cancellable — never paid, already spent,
already cancelled, or bought with a consent that gave the right away — answers
the same `409 order_not_cancellable` with the reasons in the detail.
Distinguishing them would tell a caller which orders exist.
