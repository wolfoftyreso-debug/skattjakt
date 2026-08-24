// POST /api/documents/upload-url — a token the browser uploads with, directly.
//
// # Why the bytes never come through here
//
// A Vercel function's request body is capped at 4.5 MB. That is a platform
// limit, not a setting, so a 5 GB upload cannot be a request body — not with a
// bigger memory allocation, not with a longer timeout, not ever.
//
// So it is not one. The browser asks for a token, uploads straight to Vercel
// Blob with it, and tells us the resulting URL. Vercel Blob does multipart
// internally and handles 5 GB without either side buffering it. The function's
// job is authorisation and bookkeeping: which tenant, which document, what was
// declared, what actually arrived.
import { handleUpload } from '@vercel/blob/client';
import { problem, secure } from '../../lib/http.mjs';
import { asTenant } from '../../lib/db.mjs';

export const config = { runtime: 'nodejs', maxDuration: 30 };

/** Five gigabytes, matching `MAX_DECLARED_BYTES` in the Rust store. */
const MAX_BYTES = 5 * 1024 * 1024 * 1024;

export default async function handler(req, res) {
  if (req.method !== 'POST') {
    return problem(res, 405, 'method_not_allowed', 'POST to request an upload token.');
  }
  const companyId = req.headers['x-company-id'];
  if (!companyId) {
    return problem(res, 401, 'unauthorized', 'Ange vilket bolag uppladdningen gäller.');
  }

  try {
    const body = await handleUpload({
      body: req.body,
      request: req,

      // Called before a token is issued. Everything that decides whether this
      // upload may happen belongs here, because after this the browser talks to
      // storage and we are not in the path.
      onBeforeGenerateToken: async (pathname) => ({
        // Every type. The bytes decide what a file is once it arrives, and a
        // customer with a folder of material should not have to know in advance
        // which parts we can read — see `MimeType::sniff`.
        allowedContentTypes: undefined,
        maximumSizeInBytes: MAX_BYTES,
        // Tenant-prefixed, the same shape the Rust store uses. A key that does
        // not name its tenant is a key that can be guessed into.
        addRandomSuffix: true,
        pathname: `companies/${companyId}/uploads/${pathname}`,
        tokenPayload: JSON.stringify({ companyId }),
      }),

      // Called by Vercel after the bytes have landed, not by the browser. That
      // matters: a client that says "I uploaded it" is a client claiming
      // something about its own payment, which is the mistake this whole system
      // is built not to make.
      onUploadCompleted: async ({ blob, tokenPayload }) => {
        const { companyId: tenant } = JSON.parse(tokenPayload);
        // Recorded against the same table the Rust ticket flow uses, so both
        // paths into storage leave the same trail. A second table would be a
        // second answer to "what did this tenant upload".
        await asTenant(tenant, async (tx) => {
          await tx`
            INSERT INTO upload_tickets (company_id, storage_key, declared_name,
                                        declared_type, declared_size, state,
                                        expires_at, completed_at)
            -- 'issued', not 'completed': the table's own constraint says a
            -- completed ticket names a document version, and no version exists
            -- until something reads the bytes. Claiming otherwise here would
            -- be a row that says the work is done before it started.
            VALUES (current_company_id(), ${blob.pathname},
                    ${blob.pathname.split('/').pop()},
                    ${blob.contentType ?? 'application/octet-stream'},
                    ${Math.max(1, blob.size ?? 1)}, 'issued',
                    now() + interval '30 minutes', NULL)`;
        });
      },
    });
    return secure(res).status(200).json(body);
  } catch (error) {
    console.error('upload token failed', error);
    return problem(res, 400, 'upload_rejected', error.message);
  }
}
