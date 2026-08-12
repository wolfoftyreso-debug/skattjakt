-- `notifications` is a queue, and migration 0005 put it under row-level
-- security. That was wrong, and it failed closed: the delivery worker's claim
-- matched nothing, so the outbox filled and nothing drained it — which is
-- exactly the failure a queue under RLS produces, silently.
--
-- The reasoning is the same one already written against `jobs` in migration
-- 0003, and it should have been applied here:
--
--   A queue is scanned across tenants by definition. The worker asks "what is
--   due?", not "what is due for company X" — it does not know X yet, and
--   finding out is what claiming is for. There are three ways to make that
--   work and only one is acceptable:
--
--     1. Give the worker a BYPASSRLS role. That role would also bypass
--        isolation on every table holding the customer's economy, so a single
--        forgotten WHERE clause anywhere becomes a cross-tenant leak. No.
--     2. Wrap the claim in a SECURITY DEFINER function. Workable, but it means
--        a function that can read every tenant's rows, which is the thing RLS
--        was protecting — with extra indirection and the same exposure.
--     3. Recognise that this table is a queue, take it out of RLS, and bound
--        what it is allowed to contain so that scanning it across tenants
--        discloses nothing worth protecting.
--
-- This is (3), and the bound is the part that makes it safe. `notifications`
-- holds a company id, a user id, a kind, a subject id, a channel list, state
-- and timing. It has **no payload column** — deliberately, and the comment
-- below is there so nobody adds one. A read of every row in this table reveals
-- that a company had an analysis finish and when. It does not reveal what the
-- analysis found, what the company is called, or a single figure from its
-- accounts.
--
-- The customer-facing read is scoped in the query instead, by company and by
-- user, the same way `devices` is.

DROP POLICY IF EXISTS tenant_isolation ON notifications;
ALTER TABLE notifications NO FORCE ROW LEVEL SECURITY;
ALTER TABLE notifications DISABLE ROW LEVEL SECURITY;

COMMENT ON TABLE notifications IS
    'A delivery queue, scanned across tenants by the notification worker, and '
    'therefore outside row-level security — see migration 0006. It is kept safe '
    'by what it may contain: identifiers, a kind, state and timing. Never add a '
    'payload, a message body, a company name or an amount to this table. The '
    'customer-facing read is scoped by company and user in the query.';

-- `upload_tickets` keeps its policy. It is not a queue: every access is made by
-- a caller who already knows which tenant they are, inside a tenant
-- transaction. The expiry sweep is the one cross-tenant access and it reads no
-- column it would matter to disclose.
