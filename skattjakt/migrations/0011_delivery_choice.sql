-- The choice a consumer must actually be given, rather than told about.
--
-- What this is for
-- ================
--
-- A digital service delivered immediately loses its right of cancellation only
-- if the consumer has **expressly consented** to the delivery beginning and has
-- **acknowledged** that the right is thereby lost — distansavtalslagen
-- (2005:59) 2 kap. 11 § 11 and 12 §. Publishing that on a terms page is not
-- consent. Consent is something the buyer does, at the moment of buying, and it
-- has to be recorded well enough to show afterwards what they agreed to.
--
-- `/villkor` and `/angerratt` already promised the buyer two options — start now
-- and lose the right, or wait out the fourteen days and keep it — and the
-- checkout offered neither. A purchase term describing a choice that does not
-- exist is the worst kind of documentation drift, because it is the kind a
-- customer can rely on.
--
-- Why the wording version is stored
-- =================================
--
-- Proving consent means knowing what was consented to. Storing only a boolean
-- would record that a box was ticked next to text nobody can reconstruct — the
-- wording changes, and every earlier consent silently becomes a claim about
-- words the buyer never saw. The version pins it.
--
-- Why the default is the cautious one
-- ===================================
--
-- Absent an explicit consent, a consumer keeps their right of cancellation.
-- `after_cancellation_period` is therefore the default for any row that does not
-- say otherwise, so a column added to existing data can only ever err towards
-- the buyer.

ALTER TABLE orders
    ADD COLUMN delivery_choice TEXT NOT NULL DEFAULT 'after_cancellation_period'
        CHECK (delivery_choice IN ('immediate', 'after_cancellation_period')),
    -- When the buyer consented to immediate delivery, and to which wording.
    ADD COLUMN consent_at TIMESTAMPTZ,
    ADD COLUMN consent_wording_version TEXT,
    -- The earliest moment the analysis may run. `now()` for immediate delivery;
    -- fourteen days out when the buyer chose to keep their right to cancel.
    ADD COLUMN deliverable_from TIMESTAMPTZ NOT NULL DEFAULT now();

-- Consent is recorded exactly when, and only when, immediate delivery was
-- chosen. Either direction of a mismatch is a lie about what the buyer agreed
-- to: consent without the choice, or the choice without consent.
ALTER TABLE orders ADD CONSTRAINT immediate_delivery_is_consented CHECK (
    (delivery_choice = 'immediate') = (consent_at IS NOT NULL)
);

-- And a recorded consent always names the words it was given to.
ALTER TABLE orders ADD CONSTRAINT consent_names_its_wording CHECK (
    (consent_at IS NULL) = (consent_wording_version IS NULL)
);

COMMENT ON COLUMN orders.delivery_choice IS
    'Whether the buyer consented to immediate delivery, losing the right of '
    'cancellation, or chose to wait out the fourteen days.';
COMMENT ON COLUMN orders.deliverable_from IS
    'Earliest moment the analysis this order bought may be started.';
