# @bothy-sync/node

**Node.js binding for [Bothy](https://github.com/bothy-sync/bothy), a serverless, local-first
sync engine that uses your own cloud storage as the transport.**

> ⚠️ **Early development.** This is the second step of the binding (see
> [#112](https://github.com/bothy-sync/bothy/issues/112)): it syncs through a shared folder
> and can `put`, `get`, `patch`, `list` and `delete` documents. Google Drive, change streams
> and encryption options come in the next steps. The package is not published yet.

It wraps the Rust engine directly, like the Python package does. For a format-only package
without native code, see [`@bothy-sync/core`](../js).

```js
import { Database, FolderRemote } from "@bothy-sync/node";

const db = await Database.open({
  path: "notes.db",
  appId: "com.example.notes",
  remote: new FolderRemote("/mnt/shared/notes"),
});
await db.initOrJoin();

const notes = db.collection("notes");
await notes.put("n1", { title: "Hello", stars: 3, tags: ["a", "b"] });
console.log(await notes.get("n1"));

await notes.patch("n1", { stars: 4 }, ["tags"]); // set `stars`, remove `tags`
console.log(await notes.list({ limit: 10 })); // [{ id: "n1", doc: { title: "Hello", stars: 4 } }]
await notes.delete("n1");
console.log(await db.collections()); // names of collections with at least one document

await db.sync(); // on the other device: pulls the changes
await db.close();
```

## Values

Documents are read and written without JSON, so the format's integers, floats and bytes
survive. `1` and `1.0` are the same JavaScript number, so a whole `number` is stored as an
integer.

| Writing (JavaScript) | Stored as |
|---|---|
| `number` that is a whole number up to ±(2^53 − 1) | integer |
| any other finite `number` | float |
| `bigint` within the signed 64-bit range | integer |
| `Uint8Array` (and `Buffer`) | bytes |
| `string`, `boolean`, `null`, arrays, plain objects | the same type |

These throw `BothyError` with `kind: "InvalidInput"`: `NaN`, `±Infinity`, a `number` that is a
whole number beyond ±(2^53 − 1) (use a `bigint`), a `bigint` outside the 64-bit range,
`undefined`, `Symbol`, functions, `Date`, `Map`, `Set`, class instances, other typed arrays,
arrays with holes, Proxies over any of these, strings with a lone surrogate (also in ids, names
and paths), cyclic values and nesting deeper than 32.

| Reading (stored) | JavaScript |
|---|---|
| integer within ±(2^53 − 1) | `number` |
| any other integer | `bigint`, never rounded |
| float | `number` |
| bytes | `Uint8Array` |

A float written by another binding (for example `1.0` from Python) reads as `1`, and writing
that back makes it an integer. Merging is per field, so prefer changing only the fields you
mean to change (`patch`) over reading a document and `put`ting it back.

A top-level field name must not start with `_`; those belong to Bothy.

## Errors

Every error is a `BothyError` with a `kind`: `Offline`, `AuthRequired`, `RateLimited`,
`QuotaExceeded`, `RemoteInconsistent`, `FormatTooNew`, `SchemaTooNew`, `WrongPassword`,
`KeyRequired`, `Corrupt`, `DatabaseLocked`, `InvalidInput` or `Internal`. Messages never
contain tokens, passwords or keys. An argument of the wrong type is `InvalidInput`. An exception
thrown by your own code while a document is read (a getter, a Proxy trap) is rethrown as it is.

`close()` waits for operations in flight, then releases the local database, so the same path
can be opened again right after. Calls after `close()` reject with a `BothyError`.

## Build and test

You need Rust 1.88 or newer and Node 22 or newer.

```sh
npm ci
npm run build        # release build; `npm run build:debug` for a faster one
npm test
```

License: MIT OR Apache-2.0
