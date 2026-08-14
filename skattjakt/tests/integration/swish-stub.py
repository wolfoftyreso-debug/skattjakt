#!/usr/bin/env python3
"""A stand-in for Swish, over real mutual TLS.

Why this exists
===============

`SKATTJAKT_PAYMENTS.md` §9 has said since the crate was written: the wire format
in `swish.rs` — the URL shape, the header the token arrives in, the field names,
the status strings — is written against the documented v2 Commerce API and has
never been exercised, because there is no merchant agreement. Every test until
now stopped at the point where the client would speak to Swish.

This is the same move the suites already make for object storage: MinIO is not
S3, and running against it still catches everything that would otherwise be
caught only in production. A stub cannot tell you that Swish agrees with the
documentation. It can tell you that the client sends what *this file* says
Swish expects, which turns a spec read once into a spec asserted on every run —
and it makes the mutual-TLS handshake, the certificate loading and the JSON
shapes real rather than assumed.

What is deliberately faithful
=============================

* **Mutual TLS is enforced.** `CERT_REQUIRED` with the test CA, so a client that
  fails to present its certificate is rejected at the handshake, exactly as
  Swish would reject it. This is the one part of the integration with no
  fallback: get it wrong and nothing works at all.
* The token arrives in the `paymentrequesttoken` **header**, not the body.
* Amounts come back as JSON numbers, and `payeePaymentReference` is echoed —
  the two fields settlement actually checks.
* A payment starts `CREATED` and only moves when this file is told to move it,
  because a payer approving in an app is not something a test can hurry.

What it is not
==============

Not a Swish emulator. It implements the two calls this system makes and refuses
everything else with a 404, so a route added to the client without being added
here fails loudly rather than silently passing.

Usage: swish-stub.py PORT CERTDIR
  CERTDIR must contain server.pem (cert+key) and ca.pem.

Control (plain HTTP is not offered — the control plane is the same TLS port):
  PUT  /api/v2/paymentrequests/{id}   create, as Swish
  GET  /api/v2/paymentrequests/{id}   read, as Swish
  POST /_control/{id}/{status}        move a payment (PAID, DECLINED, ERROR)
"""

import json
import ssl
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
CERTDIR = sys.argv[2]

# Everything this stub has been told about. Guarded because the server is
# threaded and the control calls race the client's polling by design.
PAYMENTS = {}
LOCK = threading.Lock()

# Every request the client made, so the suite can assert on what was sent rather
# than only on what came back. What a client *sends* is the half of a wire
# format that a stub would otherwise never check.
SEEN = []


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass  # the suite owns stdout

    def _send(self, code, body=None, headers=None):
        payload = json.dumps(body).encode() if body is not None else b""
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if payload:
            self.wfile.write(payload)

    def _instruction(self, prefix):
        if not self.path.startswith(prefix):
            return None
        rest = self.path[len(prefix):]
        return rest if rest and "/" not in rest else None

    def do_PUT(self):
        instruction = self._instruction("/api/v2/paymentrequests/")
        if instruction is None:
            return self._send(404, {"errorCode": "RP01", "errorMessage": "unknown route"})

        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length).decode()
        try:
            body = json.loads(raw)
        except ValueError:
            return self._send(422, [{"errorCode": "RP02", "errorMessage": "not json"}])

        with LOCK:
            SEEN.append({"method": "PUT", "instruction": instruction, "body": body})

        # The checks Swish itself makes, and the ones worth failing on here: an
        # instruction id that is not 32 uppercase hex, a missing payee, an
        # amount that is not a decimal string. A stub that accepted anything
        # would let a malformed request pass as a working integration.
        if len(instruction) != 32 or any(
            c not in "0123456789ABCDEF" for c in instruction
        ):
            return self._send(
                422, [{"errorCode": "RP02", "errorMessage": "bad instruction id"}]
            )
        for required in ("payeeAlias", "amount", "currency", "callbackUrl"):
            if not body.get(required):
                return self._send(
                    422,
                    [{"errorCode": "RP02", "errorMessage": f"missing {required}"}],
                )

        with LOCK:
            if instruction in PAYMENTS:
                return self._send(
                    409, [{"errorCode": "RP09", "errorMessage": "duplicate"}]
                )
            PAYMENTS[instruction] = {
                "id": instruction,
                "payeePaymentReference": body.get("payeePaymentReference"),
                "callbackUrl": body["callbackUrl"],
                "payerAlias": body.get("payerAlias"),
                "payeeAlias": body["payeeAlias"],
                "amount": float(body["amount"]),
                "currency": body["currency"],
                "message": body.get("message"),
                "status": "CREATED",
                "dateCreated": "2026-01-01T00:00:00.000Z",
                "datePaid": None,
                "errorCode": None,
                "errorMessage": None,
            }

        # Swish answers 201 with the token in a header and a Location. The token
        # is what a phone turns into an app switch.
        self._send(
            201,
            None,
            {
                "paymentrequesttoken": f"tok{instruction[:29]}",
                "location": f"https://127.0.0.1:{PORT}/api/v2/paymentrequests/{instruction}",
            },
        )

    def do_GET(self):
        instruction = self._instruction("/api/v2/paymentrequests/")
        if instruction is None:
            return self._send(404, {"errorCode": "RP01"})
        with LOCK:
            SEEN.append({"method": "GET", "instruction": instruction})
            payment = PAYMENTS.get(instruction)
        if payment is None:
            return self._send(404, {"errorCode": "PA02", "errorMessage": "not found"})
        self._send(200, {k: v for k, v in payment.items() if k != "callbackUrl"})

    def do_POST(self):
        # The control plane: what the payer would have done in the app.
        if self.path.startswith("/_control/"):
            parts = self.path[len("/_control/"):].split("/")
            if len(parts) != 2:
                return self._send(400, {"error": "usage: /_control/{id}/{status}"})
            instruction, status = parts
            with LOCK:
                payment = PAYMENTS.get(instruction)
                if payment is None:
                    return self._send(404, {"error": "no such payment"})
                payment["status"] = status
                if status == "PAID":
                    payment["datePaid"] = "2026-01-01T00:00:30.000Z"
                elif status in ("DECLINED", "ERROR"):
                    payment["errorCode"] = "BANKIDCL"
                    payment["errorMessage"] = "the payer declined"
            return self._send(200, {"status": status})

        if self.path == "/_seen":
            with LOCK:
                return self._send(200, SEEN)

        self._send(404, {"errorCode": "RP01"})


context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(f"{CERTDIR}/server.pem")
# The part that matters: a client with no certificate does not get in. Swish
# authenticates the merchant by the certificate alone — there is no API key —
# so a stub that skipped this would leave the whole authentication path
# untested.
context.verify_mode = ssl.CERT_REQUIRED
context.load_verify_locations(f"{CERTDIR}/ca.pem")

server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
server.socket = context.wrap_socket(server.socket, server_side=True)
print(f"swish stub on :{PORT}", flush=True)
server.serve_forever()
