// The analysis engine, loaded once per warm instance.
//
// Instantiating the module costs about 37 ms and every analysis after it about
// 1.7 ms, measured over fifty calls in Node. That ratio is the whole reason the
// import lives at module scope: Vercel reuses a warm instance across requests,
// so the cost is paid on a cold start and never again.
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

let engine;
export function loadEngine() {
  if (!engine) {
    // Written by scripts/build-wasm.sh during the Vercel build step. Absent
    // means the build did not run — a deployment problem, and one worth an
    // explicit message rather than a module-not-found stack.
    try {
      engine = require('./engine/skattjakt_wasm.js');
    } catch (cause) {
      throw new Error(
        'the analysis engine was not built; run scripts/build-wasm.sh (it runs ' +
        'automatically in the Vercel build step)',
        { cause },
      );
    }
  }
  return engine;
}

/**
 * Runs one analysis. Returns the report, or throws with the engine's own
 * message — which names the document and the reason, and is safe to show.
 */
export async function analyse(request) {
  const out = JSON.parse(await loadEngine().analyse(request));
  if (out.error) {
    const err = new Error(out.error);
    err.fromEngine = true;
    throw err;
  }
  return out.report;
}

export function ruleSetVersion() {
  return loadEngine().rule_set_version();
}
