// Postgres, over a serverless-friendly driver.
//
// The schema is the same one `migrations/` builds — including
// `FORCE ROW LEVEL SECURITY` and `current_company_id()`. That survives the move
// intact, and the reason is worth stating: the tenant is applied with
// `set_config('skattjakt.company_id', $1, true)`, whose third argument makes it
// **transaction-scoped**. A transaction-pooled connection (Neon's pooler,
// PgBouncer) hands a different backend to the next transaction, which would
// break a session-scoped `SET` — and does not break this one. The isolation
// model was already compatible with serverless; nobody planned that, but it
// holds.
//
// What does NOT survive: `SELECT ... FOR UPDATE SKIP LOCKED` job leasing across
// a pooled connection is fine, but the worker that ran it is gone. See
// `api/cron/reconcile.mjs`.
import { neon, neonConfig } from '@neondatabase/serverless';

neonConfig.fetchConnectionCache = true;

let sql;
export function db() {
  if (!sql) {
    const url = process.env.DATABASE_URL;
    if (!url) {
      // Deliberately fatal rather than "running statelessly". A deployment with
      // no database that still takes orders is a deployment that loses them.
      throw new Error('DATABASE_URL is not set');
    }
    sql = neon(url);
  }
  return sql;
}

/**
 * Runs a unit of work as one tenant, in one transaction.
 *
 * Every statement inside sees only that company's rows, enforced by the
 * database rather than by the query remembering a `WHERE`. The `set_config`
 * is the first statement and is transaction-scoped, so nothing leaks to the
 * next user of a pooled connection.
 */
export async function asTenant(companyId, work) {
  const client = db();
  return client.transaction(async (tx) => {
    await tx`SELECT set_config('skattjakt.company_id', ${companyId}, true)`;
    return work(tx);
  });
}
