import test, { before, after } from "node:test";
import assert from "node:assert/strict";
import { BothyError } from "../index.js";
import { env } from "./helpers.js";

const e = env();
let db;

before(async () => {
  db = await e.open("crud");
  await db.initOrJoin();
});

after(async () => {
  await db.close();
  await e.cleanup();
});

const invalid = (err) => err instanceof BothyError && err.kind === "InvalidInput";

test("patch sets the given fields and keeps the others", async () => {
  const c = db.collection("patch");
  await c.put("a", { title: "t", done: false });
  assert.equal(await c.patch("a", { done: true }), true);
  assert.deepStrictEqual(await c.get("a"), { title: "t", done: true });
  assert.equal(await c.patch("a", { done: true }), false, "nothing changed");
});

test("patch removes the fields in unset and creates a missing document", async () => {
  const c = db.collection("patch");
  await c.put("b", { x: 1, y: 2, z: 3 });
  await c.patch("b", { y: 20 }, ["x", "missing"]);
  assert.deepStrictEqual(await c.get("b"), { y: 20, z: 3 });
  assert.equal(await c.patch("new", { n: 1n << 60n }), true);
  assert.deepStrictEqual(await c.get("new"), { n: 1n << 60n });
});

test("patch rejects bad input with InvalidInput", async () => {
  const c = db.collection("patch");
  await assert.rejects(c.patch("a", { v: NaN }), invalid);
  await assert.rejects(c.patch("a", [1]), invalid);
  await assert.rejects(c.patch("", { a: 1 }), invalid);
  await assert.rejects(c.patch("a", {}, "x"), invalid);
  await assert.rejects(c.patch("a", {}, [1]), invalid);
  await assert.rejects(c.patch("a", {}, ["\ud800"]), invalid);
  await assert.rejects(c.patch("a", { _hidden: 1 }), invalid);
});

test("delete reports whether the document existed", async () => {
  const c = db.collection("del");
  await c.put("a", { v: 1 });
  assert.equal(await c.delete("a"), true);
  assert.equal(await c.get("a"), null);
  assert.equal(await c.delete("a"), false);
  assert.equal(await c.delete("never"), false);
  await assert.rejects(c.delete(""), invalid);
  await c.put("a", { v: 2 });
  assert.deepStrictEqual(await c.get("a"), { v: 2 }, "a deleted id can be written again");
});

test("list returns documents in id order, with limit and after", async () => {
  const c = db.collection("list");
  for (const id of ["c", "a", "e", "b", "d"]) await c.put(id, { id, big: 2n ** 60n });
  const all = await c.list();
  assert.deepStrictEqual(all.map((r) => r.id), ["a", "b", "c", "d", "e"]);
  assert.deepStrictEqual(all[0], { id: "a", doc: { id: "a", big: 2n ** 60n } });
  const page = await c.list({ limit: 2 });
  assert.deepStrictEqual(page.map((r) => r.id), ["a", "b"]);
  const next = await c.list({ limit: 2, after: "b" });
  assert.deepStrictEqual(next.map((r) => r.id), ["c", "d"]);
  assert.deepStrictEqual(await c.list({ after: "e" }), []);
  assert.deepStrictEqual(await db.collection("empty").list(), []);
  await c.delete("a");
  assert.deepStrictEqual((await c.list()).map((r) => r.id), ["b", "c", "d", "e"]);
});

test("list rejects bad options", async () => {
  const c = db.collection("list");
  for (const bad of [{ limit: -1 }, { limit: 1.5 }, { limit: "2" }, { limit: NaN }, { after: 1 }, { after: "" }, "x", 5]) {
    await assert.rejects(c.list(bad), invalid, JSON.stringify(bad));
  }
  assert.deepStrictEqual(await c.list(undefined), await c.list());
  assert.deepStrictEqual(await c.list(null), await c.list());
  assert.deepStrictEqual(await c.list({}), await c.list());
});

test("collections lists the names that hold a visible document", async () => {
  const d = await e.open("names");
  await d.initOrJoin();
  assert.deepStrictEqual(await d.collections(), []);
  await d.collection("zeta").put("1", { a: 1 });
  await d.collection("alpha").put("1", { a: 1 });
  await d.collection("gone").put("1", { a: 1 });
  await d.collection("gone").delete("1");
  assert.deepStrictEqual((await d.collections()).sort(), ["alpha", "zeta"]);
});

test("patch, delete and list reach the other device through sync", async () => {
  const a = await e.open("sa");
  const b = await e.open("sb");
  await a.initOrJoin();
  await b.initOrJoin();
  await a.collection("s").put("1", { keep: 1, drop: 2 });
  await a.collection("s").put("2", { v: 1 });
  await a.sync();
  await b.sync();
  await a.collection("s").patch("1", { added: true }, ["drop"]);
  await a.collection("s").delete("2");
  await a.sync();
  await b.sync();
  assert.deepStrictEqual(await b.collection("s").list(), [{ id: "1", doc: { keep: 1, added: true } }]);
});

test("the new calls reject with BothyError after close()", async () => {
  const d = await e.open("closed");
  await d.initOrJoin();
  const c = d.collection("c");
  await d.close();
  for (const run of [() => c.patch("a", { v: 1 }), () => c.delete("a"), () => c.list(), () => d.collections()]) {
    await assert.rejects(run(), (err) => err instanceof BothyError);
  }
});
