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

Four things are refusals to start rather than warnings, because each one
produces a deployment that takes orders it cannot collect on:

- a Swish number that is not `123` + seven digits;
- `SKATTJAKT_PAYMENTS_REQUIRED` with no provider configured;
- a certificate file that cannot be read or parsed;
- a callback URL that is not `https://`.

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
on one order cannot both observe `paid`. The loser updates nothing and receives
`402`. A unique index on `analysis_id` makes a second attempt a failed
transaction rather than a support ticket.

A check-then-act in the handler passes every sequential test and fails exactly
where it matters — a customer double-tapping on a slow connection.
`tests/integration/payments.sh` fires ten concurrent requests at one paid order
and asserts that exactly one wins.

The redemption happens **inside the transaction that creates the analysis**, so
an order cannot be spent on an analysis that then fails to be created.

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
| Verified against a real database and a real API | Yes — `tests/integration/payments.sh`, 25 checks |
| Settlement logic | 24 unit tests in `crates/payments` |

The wire format in `swish.rs` — URLs, field names, status strings — is written
against the documented v2 Commerce API and **must be checked against the
specification the bank supplies**. It is concentrated in two structs (`Wire`,
`WirePayment`) precisely so that checking it is reading two structs rather than
auditing a client.

Until a payment has been made against the test host with a real certificate, the
honest description of this is: the gate, the state machine, the double-spend
defence and the settlement rules are tested and hold; the conversation with
Swish has never happened.
