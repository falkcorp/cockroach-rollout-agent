- [ ] **ui-ca-in-db** Keep the UI CA key in the coordination DB, encrypted
      with an operator password, so any node can renew instead of only the
      node that ran `ui-cert init-ca`. Generate a fresh CA key on every
      renewal. Asked for by the operator on 2026-10-12. For now the CA key
      lives on one node.
- [ ] **crdb-internal-alternatives** Replace `crdb_internal.gossip_nodes`
      and `kv_store_status` reads with supported interfaces. CockroachDB
      26.1 gates them behind `allow_unsafe_internals`, which the agent now
      sets on every session.
