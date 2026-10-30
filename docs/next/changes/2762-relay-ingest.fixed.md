- Bind cached relay rows to the sender identity proven by their poll or enrolled
  inbound edge. Reject relays from mismatched poll identities, and prevent alias
  rebinding from lending a new owner identity to old cached rows. A new sender
  evicts rows from the previous sender. Share one poll observation instant with
  its direct and relayed readings, so freshness ordering has no clock skew.
