- Bind cached relay rows to the sender identity proven by their poll or enrolled
  inbound edge. Refuse owner proof from mismatched poll identities, and prevent
  alias rebinding from lending a new owner identity to old cached rows. A new
  sender evicts rows from the previous sender. Share one poll observation instant
  with its direct and relayed readings, so freshness ordering has no clock skew.
- Revoke cached relay proof when its current binding disappears, including when
  a failed pin-store read leaves no binding. Deliveries without a validated
  sender retain only unproven observations and evict that alias's previously
  proven relay rows.
