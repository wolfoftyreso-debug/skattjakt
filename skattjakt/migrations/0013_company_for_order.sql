-- One question the analysis function has to ask before it knows a tenant.
--
-- `POST /api/analyse` receives an order id and must find the company that owns
-- it before it can open a tenant-scoped transaction to redeem it. That is the
-- same shape as `company_for_payment_reference` in 0012, and it exists for the
-- same reason: `orders` is FORCE RLS keyed on `current_company_id()`, so
-- nothing can read it until the tenant is already known, and here it is not.
--
-- The escape hatch is narrow on purpose:
--
--   1. SECURITY DEFINER, so it runs as the owner and sees past RLS.
--   2. A pinned `search_path`, so no temp object can shadow what it reads.
--   3. It answers one question and returns one id. It cannot be asked for an
--      amount, a state, or anything a caller could use to enumerate orders.
--   4. REVOKEd from PUBLIC and granted only to the application role.
--
-- An unknown or forged order id returns NULL, which the caller treats as "no
-- such order" — the same answer a wrong guess deserves.

CREATE FUNCTION company_for_order(p_order_id UUID)
RETURNS UUID
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT company_id FROM orders WHERE id = p_order_id;
$$;

REVOKE ALL ON FUNCTION company_for_order(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION company_for_order(UUID) TO skattjakt_app;
