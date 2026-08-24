// GET /api/cron/reconcile — what the analysis worker's sweep used to do.
//
// The worker leased jobs off a Postgres queue in a loop. Vercel runs no loops,
// so the sweep becomes a cron function: same query, same settlement path, one
// pass per firing instead of one every thirty seconds.
//
// The callback is the optimisation and this is the guarantee. That was already
// true in the Rust service — the callback spent a while resolving nothing and
// no payment was lost, because the sweep caught every one of them.
import { settleByReference } from '../../lib/settle.mjs';
import { db } from '../../lib/db.mjs';
import { secure } from '../../lib/http.mjs';

export const config = { runtime: 'nodejs', maxDuration: 60 };

/** Bounded so a backlog is worked through over several firings rather than
 *  timing out the function and completing none of it. */
const PER_SWEEP = 50;

export default async function handler(req, res) {
  // Vercel signs cron invocations with this header. Checked because the route
  // is otherwise a way for anyone to make us call Swish fifty times.
  if (req.headers.authorization !== `Bearer ${process.env.CRON_SECRET}`) {
    return secure(res).status(401).end();
  }

  const rows = await db()`
    SELECT reference FROM unsettled_payments(interval '90 seconds', ${PER_SWEEP})`;
  const counts = { accepted: 0, unknown: 0, failed: 0 };
  for (const { reference } of rows) {
    try {
      counts[await settleByReference(reference)] += 1;
    } catch (error) {
      counts.failed += 1;
      console.error('sweep could not settle', reference, error.message);
    }
  }
  console.info(JSON.stringify({ event: 'reconcile', examined: rows.length, ...counts }));
  return secure(res).status(200).json({ examined: rows.length, ...counts });
}
