# Operator client entry (ADR-0036 §5)

Build with the `web` feature and explicitly configure the structured GUI endpoint:

```toml
[client]
bind = "127.0.0.1:7682"
```

The endpoint is off by default and separate from `[web]`, the terminal bridge.
Only IPv4 `127.0.0.1` with a nonzero port is accepted. This slice supplies the
exchange router; #1828 mounts it and the GUI on the configured listener.
Until that listener is delivered, opening its URL cannot connect.

Run these commands outside agent panes as the local operator:

```sh
flk client token add phone --scopes read,send
flk client token list
flk client token rm phone
flk client token rm --entry HEX_ID
flk client open
flk client open --print
```

`token add` prints the bearer to terminal stdout. Redirected or piped stdout
requires `--json` or `--show-token`; without either it refuses before issuing a
credential. The explicit flag allows the usual issuance response to contain the
bearer even in a pipe or file. Treat that destination as secret storage.
List returns public metadata and identifies unsafe entries without their contents;
`rm --entry` revokes an unsafe entry by its lossless hexadecimal filename identifier.
Token files are mode 0600 under a mode-0700 `client-tokens` directory. Scopes are
exact grants: `operate` does not implicitly grant `read`, `send`, or `answer`.
Agents, relays, and callers lacking the required kernel identity are refused with
`operator_only` and exit 13. Structured starts are not added by these commands.

`client open` creates or reuses the reserved `operator` token with all four explicit
grants and opens a new browser tab or window through the platform URL opener.
An existing token under that name with different
grants is refused rather than widened; revoke it explicitly before issuing a new
operator credential. `--print` prints the URL instead of launching a browser;
it prints only the URL even with `--json`. Without `--print`, `--json` preserves
the socket response envelope. The URL contains only a random
code in its fragment, for example `http://127.0.0.1:7682/#code=…`. Treat this URL
as a temporary credential. It contains neither the bearer nor a query parameter.

The GUI consumes the fragment and exchanges the code through a same-origin
`POST /v1/client/exchange` with JSON `{"code":"…"}`. The response is
`{"token":"…"}` with `Cache-Control: no-store`. It uses that bearer in its
normal WebSocket hello. GUI fragment consumption is delivered by the GUI slice.
The code uses 64 lowercase hex characters, requiring no fragment escaping.
The served client derives its server URL and WebSocket transport from its own
location (`ws://<host>/v1/ws` on this HTTP loopback listener); no config script
is served. Each 256-bit code is retained only in server memory, indexed by its SHA-256 hash,
bound to one token, usable once, and invalid at age 60 seconds. Restart drops all
codes; mint again. Token removal or replacement invalidates an outstanding code.
Cross-origin, ambiguous Host/Origin, and non-loopback peer requests are refused.
Host and Origin must identify the configured numeric loopback authority (HTTP's
default port is normalized); forwarding headers grant no authority.

Used, expired, unknown, and revoked codes receive the same 401 body. Two
concurrent exchanges of one code yield exactly one bearer. The registry holds
at most 128 live codes, and request bodies are capped at 1024 bytes. There is
no global failure budget: local processes can forge Host/Origin and could use
such a gate to lock out the operator. Guessing a 256-bit, single-use code within
60 seconds is infeasible. Codes and tokens never enter diagnostic
logs; browser-launch argv redacts the code fragment. Tokens remain persisted
until explicitly revoked. #1870 tracks admission/revocation lock fairness,
waiter cleanup, and moving exchange token-store reads off the async executor.
It must be fixed before #1828 exposes admission.
