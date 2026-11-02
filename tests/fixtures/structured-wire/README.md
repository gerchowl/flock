# Server-driven fleet script

`fleet-server.jsonl` drives the registry owner and real authenticated WebSocket
endpoint in `web::wire::tests::fleet`. `add` and `remove` change the structured
provider, `sub` and `unsub` send actual client frames, and `expect` selects fields
from the next server-produced frame. No server frame is injected. Every receive
has the shared five-second failure ceiling and rejects socket closure.

`rows` pins the materialized membership: absent rows survive the start of a
nonempty replacement and disappear at `snapshot_to`, before the synced barrier.
An empty checkpoint clears membership immediately. Rows and deltas decode through
the generated FleetAgentRow/FleetDelta contract. The existing client-side
`web/client/test/fixtures/wire/fleet-lite.jsonl` remains a separate client fixture.

`web/client/tests/wire/fleet-server.ts` also connects the real WireConnection to
the isolated endpoint. Its strict stdout/stdin stages trigger provider mutations,
then verify that the real client retains absent rows during replacement and
removes them at completion, including an empty checkpoint. Unexpected stages,
EOF and a five-second silent-producer ceiling fail; there are no success sleeps.
