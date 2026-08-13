-- What an analysis was bought as.
--
-- The hole this closes
-- ====================
--
-- The payment gate checked *that* an order was paid. It never checked *what
-- the order was for*. The report endpoint chose its presentation layer from a
-- query parameter:
--
--     GET /v1/analyses/{id}/report?audience=accountant
--
-- so a customer who paid 29 kronor for Privatanalys could ask for — and
-- receive — the 69-kronor Skattjakt Kontroll report, control review and all.
-- The money was verified and the entitlement was not, which is the same class
-- of mistake as letting the client declare its own payment settled: the client
-- was deciding what it had bought.
--
-- The fix is to record the entitlement where the client cannot reach it, at the
-- moment the order is redeemed and in the same transaction, and to let the
-- report read it rather than the query string.
--
-- Why nullable
-- ============
--
-- An analysis created while `SKATTJAKT_PAYMENTS_REQUIRED` is off was not bought
-- at all, and inventing a product for it would be a lie in the one column whose
-- whole purpose is to say what was paid for. NULL means exactly what it looks
-- like: nobody bought this, so nothing constrains it. Every analysis that ever
-- ran through the gate has a value, and the constraint below is what keeps that
-- true rather than a convention someone has to remember.

ALTER TABLE analysis_jobs
    ADD COLUMN audience TEXT
    CHECK (audience IN ('private', 'company', 'accountant'));

COMMENT ON COLUMN analysis_jobs.audience IS
    'The presentation layer this analysis was bought as, set from the order at '
    'redemption. NULL when payments were not required and nothing was bought.';

-- An order that has been redeemed must have stamped its analysis. Without this
-- a redemption could half-succeed — order consumed, analysis unstamped — and
-- the unstamped analysis would then be readable as any audience at all, which
-- is precisely the hole this migration exists to close.
--
-- Written as a trigger rather than a CHECK because it spans two tables. It is
-- deliberately a hard failure: a redemption that cannot stamp its analysis must
-- roll back and leave the order spendable, not consume the order silently.
CREATE OR REPLACE FUNCTION analysis_is_stamped_when_bought() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state = 'consumed' AND NEW.analysis_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1 FROM analysis_jobs
            WHERE id = NEW.analysis_id AND audience IS NOT NULL
        ) THEN
            RAISE EXCEPTION
                'order % was consumed but analysis % carries no audience',
                NEW.id, NEW.analysis_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- DEFERRABLE so the two statements of a redemption may run in either order
-- within the transaction; the check is what must hold at commit, not the
-- sequence that got there.
CREATE CONSTRAINT TRIGGER orders_stamp_their_analysis
    AFTER INSERT OR UPDATE ON orders
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION analysis_is_stamped_when_bought();
