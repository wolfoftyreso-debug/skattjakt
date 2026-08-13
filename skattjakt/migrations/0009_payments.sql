-- Paying for one analysis.
--
-- The invariant this schema exists to enforce
-- ===========================================
--
-- **One paid order buys exactly one analysis.** Not zero, not two.
--
-- That is easy to say in a request handler and easy to lose there: two requests
-- arriving together both read `state = 'paid'`, both decide they may proceed,
-- and one payment buys two analyses. So the rule is written where concurrent
-- transactions cannot both win — a unique constraint on the analysis an order
-- redeemed, and a state transition that must observe `paid` in the same
-- statement that leaves it.
--
-- The handler still checks, because a clear error beats a constraint violation.
-- But the handler is the message, not the guarantee.
--
-- Why the money is here at all
-- ============================
--
-- The charged amount is copied onto the order rather than looked up from the
-- product when needed. A price change must never rewrite what somebody was
-- asked to pay, and an order from March has to be answerable in November with
-- March's price.

CREATE TABLE orders (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id      UUID        NOT NULL REFERENCES companies(id) ON DELETE CASCADE,

    product         TEXT        NOT NULL
                                CHECK (product IN ('private_analysis', 'company_analysis', 'control_review')),

    -- What we asked for, in öre. Never recomputed.
    amount_ore      BIGINT      NOT NULL CHECK (amount_ore > 0),
    currency        CHAR(3)     NOT NULL DEFAULT 'SEK' CHECK (currency = 'SEK'),

    state           TEXT        NOT NULL DEFAULT 'created'
                                CHECK (state IN ('created', 'awaiting_payment', 'paid',
                                                 'declined', 'failed', 'consumed', 'refund_owed')),

    -- The analysis this order bought. Set exactly once, when the order is
    -- redeemed.
    analysis_id     UUID        REFERENCES analysis_jobs(id) ON DELETE SET NULL,

    -- Why an order failed, for the customer and for an operator.
    note            TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    paid_at         TIMESTAMPTZ,
    consumed_at     TIMESTAMPTZ,

    -- An order that has been consumed names what it bought, and one that has
    -- not names nothing. Without this an order could be marked consumed with no
    -- analysis behind it — a customer charged for nothing, invisibly.
    CONSTRAINT consumed_names_its_analysis CHECK (
        (state = 'consumed') = (analysis_id IS NOT NULL)
    ),

    -- Likewise: `paid` and later states are the only ones that can carry a
    -- payment time, and they must.
    CONSTRAINT paid_states_are_timestamped CHECK (
        (state IN ('paid', 'consumed', 'refund_owed')) = (paid_at IS NOT NULL)
    )
);

-- One analysis is bought by at most one order. This is the constraint that
-- makes double-spending a failed transaction rather than a support ticket.
CREATE UNIQUE INDEX orders_one_analysis_each
    ON orders (analysis_id) WHERE analysis_id IS NOT NULL;

CREATE INDEX orders_by_company ON orders (company_id, created_at DESC);
-- The reconciliation sweep's query: orders waiting on a payer.
CREATE INDEX orders_awaiting ON orders (state, updated_at)
    WHERE state IN ('created', 'awaiting_payment');

-- ---------------------------------------------------------------------------
-- Payments
-- ---------------------------------------------------------------------------
--
-- An order may have several payment attempts — a payer who declines and tries
-- again — so this is a separate table rather than columns on `orders`.
--
-- Deliberately absent: anything identifying the payer. Swish returns the
-- payer's phone number on a completed payment. We do not need it, so we do not
-- store it: the least surprising thing to find in a breach is what was never
-- collected. The analysis is tied to the order, and the order to the company.

CREATE TABLE payments (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id            UUID        NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    company_id          UUID        NOT NULL REFERENCES companies(id) ON DELETE CASCADE,

    provider            TEXT        NOT NULL,

    -- Our id for this attempt at the provider. For Swish this is the
    -- instruction id, derived from the payment row so a retried create sends
    -- the same one and Swish treats it as the same payment.
    provider_reference  TEXT        NOT NULL,

    state               TEXT        NOT NULL DEFAULT 'pending'
                                    CHECK (state IN ('pending', 'paid', 'declined', 'failed')),

    -- What we asked the payer for. Compared against what the provider says was
    -- actually paid before an order is ever accepted.
    amount_ore          BIGINT      NOT NULL CHECK (amount_ore > 0),

    -- What the provider last told us, verbatim, for audit. Not read by any
    -- decision — every decision goes through a fresh lookup.
    provider_status     TEXT,
    error_code          TEXT,

    -- How many times the provider has been asked about this payment. A payment
    -- nobody ever resolves is a bug somewhere, and this is what makes it
    -- visible rather than merely absent.
    lookups             INT         NOT NULL DEFAULT 0 CHECK (lookups >= 0),

    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    settled_at          TIMESTAMPTZ,

    CONSTRAINT settled_states_are_timestamped CHECK (
        (state <> 'pending') = (settled_at IS NOT NULL)
    )
);

-- The provider's reference is how a callback finds its payment, so it must
-- name exactly one. A duplicate here would mean a callback could settle the
-- wrong order.
CREATE UNIQUE INDEX payments_by_provider_reference
    ON payments (provider, provider_reference);

CREATE INDEX payments_by_order ON payments (order_id, created_at DESC);
-- The reconciliation sweep: payments nobody has resolved.
CREATE INDEX payments_pending ON payments (state, updated_at) WHERE state = 'pending';

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------
--
-- Both tables carry a company_id and take the same policy as every other tenant
-- table. The callback path is the interesting case: a callback arrives with no
-- session and no company, so it cannot open a tenant transaction. It therefore
-- runs one query — "which company owns this provider reference" — as a
-- deliberate, single, auditable exception, and does everything else inside that
-- company's tenant scope. `SKATTJAKT_PAYMENTS.md` §4 explains why that is
-- narrower than it sounds: the reference is unguessable, and knowing it lets a
-- caller cause a lookup against Swish and nothing else.

ALTER TABLE orders   ENABLE ROW LEVEL SECURITY;
ALTER TABLE orders   FORCE  ROW LEVEL SECURITY;
ALTER TABLE payments ENABLE ROW LEVEL SECURITY;
ALTER TABLE payments FORCE  ROW LEVEL SECURITY;

CREATE POLICY orders_tenant ON orders
    USING (company_id = current_company_id())
    WITH CHECK (company_id = current_company_id());

CREATE POLICY payments_tenant ON payments
    USING (company_id = current_company_id())
    WITH CHECK (company_id = current_company_id());

GRANT SELECT, INSERT, UPDATE ON orders, payments TO skattjakt_app;
