# objectrecords

> **Object Records** — Internal immutable data store for the Creo ecosystem.

## What

The data plane of the Creo ecosystem.

In Creo, *thoughts and decisions* live in [creo-memories](https://github.com/chronista-club/creo-memories) (mutable, interpretive).
*Records* — logs, FIX'd documents, datasets, and assets — live here, immutable and content-addressable.

## Status

**Design phase.** Not yet ready for use.

The architecture follows a 3-layer separation: `Creo` (mutable, interpretive) /
`ObjectRecords` (immutable records) / `Reference` (external pointers).
See [CLAUDE.md](CLAUDE.md) for the core model and design principles.

## Brand

`Object Records` originates as a record label founded by [@mako-357](https://github.com/mako-357) in 2008.
The double entendre — *records of music* → *records of data* — frames the namespace.

Domain: [objectrecords.io](https://objectrecords.io)

## Related

- [creo-memories](https://github.com/chronista-club/creo-memories) — semantic layer (mutable thoughts)
- [creo-id](https://github.com/chronista-club/creo-id) — OIDC authorization server
- [creo-ui](https://github.com/chronista-club/creo-ui) — design system
- [chronista-hub](https://github.com/chronista-club/chronista-hub) — meta-registry
