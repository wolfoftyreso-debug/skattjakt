-- Letting settlement find the payment it is settling.
--
-- The defect
-- ==========
--
-- `payments` is `FORCE ROW LEVEL SECURITY` with a policy of
-- `company_id = current_company_id()`. Two queries have to run *before* the
-- company is known, because finding it is what they are for:
--
--   * the callback resolving a provider reference to its tenant, and
--   * the reconciliation sweep listing payments nobody has settled.
--
-- Both ran against the pool with no company set, so `current_company_id()` was
-- NULL, the policy compared `company_id = NULL`, and both returned **nothing,
-- always**. Not sometimes and not for one tenant: settlement was structurally
-- impossible. A customer could pay, Swish could confirm it, and the order would
-- sit at `awaiting_payment` until somebody looked.
--
-- Nothing caught it because every test marked orders paid with `psql` as a
-- superuser, which bypasses RLS — so the tests exercised the settlement logic
-- without ever exercising the lookup that reaches it. It took a real
-- conversation over mutual TLS, in `tests/integration/swish-wire.sh`, for the
-- silence to become visible.
--
-- The fix, and the two that were rejected
-- =======================================
--
--   1. Give the application a BYPASSRLS role. It would bypass isolation on
--      every table holding a customer's economy, so one forgotten WHERE clause
--      anywhere becomes a cross-tenant leak. No.
--   2. Take `payments` out of RLS. It holds what each tenant was charged. No.
--   3. Two narrow SECURITY DEFINER functions that answer only the questions
--      settlement asks, and nothing else.
--
-- These are (3), and they are the same shape as `memberships_for_user` in
-- `0004`. `search_path` is pinned so a caller cannot shadow `payments` with a
-- table of their own, which is the classic SECURITY DEFINER escalation.
--
-- What they can and cannot tell a caller
-- ======================================
--
-- The first takes a provider reference — a 32-character instruction id this
-- system generated — and returns a company id. Someone who already knows a
-- reference learns which tenant it belongs to; they cannot list references, and
-- they cannot read an amount, an order or a document through it.
--
-- The second returns only what the sweep needs to go and ask the provider: the
-- tenant, the payment, and the reference. No amounts.

CREATE FUNCTION company_for_payment_reference(p_provider TEXT, p_reference TEXT)
RETURNS UUID
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT company_id FROM payments
    WHERE provider = p_provider AND provider_reference = p_reference
$$;

CREATE FUNCTION unsettled_payments(p_older_than INTERVAL, p_limit INT)
RETURNS TABLE (company_id UUID, payment_id UUID, provider TEXT, provider_reference TEXT)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT p.company_id, p.id, p.provider, p.provider_reference
    FROM payments p
    JOIN orders o ON o.id = p.order_id
    WHERE p.state = 'pending'
      AND o.state IN ('created', 'awaiting_payment')
      AND p.created_at < now() - p_older_than
    ORDER BY p.created_at
    LIMIT p_limit
$$;

-- Callable by the application, and by nobody else. A SECURITY DEFINER function
-- granted to PUBLIC is a hole with a nice interface.
REVOKE ALL ON FUNCTION company_for_payment_reference(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION unsettled_payments(INTERVAL, INT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION company_for_payment_reference(TEXT, TEXT) TO skattjakt_app;
GRANT EXECUTE ON FUNCTION unsettled_payments(INTERVAL, INT) TO skattjakt_app;
