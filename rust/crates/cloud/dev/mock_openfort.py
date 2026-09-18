"""A stand-in for the Openfort endpoints pay-cloud's driver touches.

Lets the whole onboarding and connector flow run on one machine with no
Openfort account: the consent page redirects straight back with a mock
grant, and the provisioning calls answer with one fixed wallet.

    python3 mock_openfort.py 8499

Point pay-cloud at it with OPENFORT_BASE_URL and OPENFORT_AUTH_PAGE_URL
(both http://127.0.0.1:8499). Credentials it hands out sign nothing real.
"""
import json
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

ADDRESS = "C78fUoBw1YDJDmzNx7viRZFnuhku3t3eiy9eiV2hafff"
WALLET = {"id": "acc_mock1", "address": ADDRESS, "chainType": "SVM", "custody": "Developer"}


class Handler(BaseHTTPRequestHandler):
    def _json(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _body(self):
        n = int(self.headers.get("content-length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def log_message(self, fmt, *args):
        sys.stderr.write("mock-openfort: " + (fmt % args) + "\n")

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        # The dashboard's consent page: no sign-in, straight back with a grant
        # in the fragment, exactly the shape the real page uses.
        if url.path == "/oauth/consent":
            q = urllib.parse.parse_qs(url.query)
            redirect = q.get("redirect_uri", [""])[0]
            state = q.get("state", [""])[0]
            if not redirect or not state:
                return self._json(400, {"error": "redirect_uri and state are required"})
            fragment = urllib.parse.urlencode(
                {
                    "api_key": "sk_test_mock",
                    "publishable_key": "pk_test_mock",
                    "project_id": "pro_mock",
                    "project": "Mock project",
                    "state": state,
                }
            )
            self.send_response(302)
            self.send_header("location", f"{redirect}#{fragment}")
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if url.path == "/v2/accounts/acc_mock1":
            return self._json(200, WALLET)
        if url.path.startswith("/v2/accounts"):
            return self._json(200, {"data": [WALLET]})
        return self._json(404, {"error": {"message": "unknown " + self.path}})

    def do_POST(self):
        auth = self.headers.get("authorization", "")
        body = self._body()
        if not auth.startswith("Bearer sk_"):
            return self._json(401, {"error": {"message": "invalid api key"}})
        if self.path == "/v2/accounts/backend/register-secret":
            ok = (
                body.get("publicKey", "").startswith("-----BEGIN PUBLIC KEY-----")
                and body.get("keyId", "").startswith("ws_")
                and body.get("walletAuthToken", "").count(".") == 2
            )
            return self._json(200 if ok else 400, {"ok": ok})
        if self.path == "/v2/accounts/backend":
            ok = body == {"chainType": "SVM"} and self.headers.get("x-wallet-auth", "").count(".") == 2
            return self._json(200, WALLET) if ok else self._json(400, {"error": {"message": "bad request"}})
        return self._json(404, {"error": {"message": "unknown " + self.path}})

    def do_PUT(self):
        body = self._body()
        if self.path == "/v1/project/apikey":
            ok = body.get("type") == "pk_wallet" and bool(body.get("uuid"))
            return self._json(200 if ok else 400, {"ok": ok})
        return self._json(404, {})


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8499
    sys.stderr.write(f"mock-openfort: listening on http://127.0.0.1:{port}\n")
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
