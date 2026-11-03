## Unreleased

Second step ([#112](https://github.com/bothy-sync/bothy/issues/112)): `collection.patch`,
`delete`, `list` and `database.collections`, and a test that the CLI and the binding share a
database. The first step follows below.

First step of the Node.js binding ([#112](https://github.com/bothy-sync/bothy/issues/112)).
Not published yet: the package stays `"private": true` until the last step.

- `Database.open`, `initOrJoin`, `collection(name).put/get`, `sync` and `close` over a
  `FolderRemote`.
- Documents are converted to Bothy values without JSON: integers and floats stay apart,
  `bigint` and `Uint8Array` are supported.
- Every error is a `BothyError` with a `kind`.
