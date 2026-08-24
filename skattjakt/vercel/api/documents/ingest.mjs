// POST /api/documents/ingest — read an uploaded file, bounded, and analyse it.
//
// # The three limits this sits between
//
//   storage      5 GB   Vercel Blob, multipart, the browser's problem
//   reading      64 MB  what the extractor will hold — see ExtractionBudget
//   the module    4 GiB  a wasm32 address space, and far less in practice
//
// So this fetches a **range**, not a file. The first 64 MB of a 5 GB export is
// the part a set of accounts is in; the rest is read by nobody and the report
// says so rather than leaving the reader to assume it was all considered.
import { analyse } from '../../lib/engine.mjs';
import { problem, secure } from '../../lib/http.mjs';

export const config = { runtime: 'nodejs', maxDuration: 60, memory: 3009 };

/** Matches `ExtractionBudget::DEFAULT_MAX_BYTES` in the Rust extractor. */
const READ_BYTES = 64 * 1024 * 1024;

export default async function handler(req, res) {
  if (req.method !== 'POST') {
    return problem(res, 405, 'method_not_allowed', 'POST the blob URL to analyse.');
  }
  const { url, filename, profile, audience, accounts_state: accountsState } = req.body ?? {};
  if (typeof url !== 'string' || !url.startsWith('https://')) {
    return problem(res, 400, 'no_document', 'Ange url till det uppladdade underlaget.');
  }

  let bytes;
  try {
    // A ranged GET. Without the header a 5 GB blob is 5 GB in this process,
    // and the function is killed before the analysis starts.
    const response = await fetch(url, {
      headers: { range: `bytes=0-${READ_BYTES - 1}` },
    });
    if (!response.ok && response.status !== 206) {
      return problem(res, 502, 'storage_unavailable',
        `Underlaget kunde inte hämtas (${response.status}).`);
    }
    bytes = Buffer.from(await response.arrayBuffer());
  } catch (error) {
    console.error('could not read the uploaded document', error);
    return problem(res, 502, 'storage_unavailable', 'Underlaget kunde inte hämtas.');
  }

  try {
    const report = await analyse({
      documents: [{
        filename: filename ?? url.split('/').pop() ?? 'underlag',
        content_base64: bytes.toString('base64'),
      }],
      profile,
      audience,
      accounts_state: accountsState,
    });
    return secure(res).status(200).json({ report });
  } catch (error) {
    if (error.fromEngine) {
      return problem(res, 422, 'analysis_failed', error.message);
    }
    console.error('analysis failed', error);
    return problem(res, 500, 'internal_error', 'Analysen kunde inte genomföras.');
  }
}
