import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { join } from "node:path";
import { APP, env } from "./helpers.js";

const cli = process.env.BOTHY_CLI;

// CI builds the CLI and sets BOTHY_CLI; without it the test is skipped, like the Python one.
test("the CLI and the Node binding share a database", { skip: !cli && "BOTHY_CLI is not set" }, async (t) => {
  const e = env();
  t.after(e.cleanup);
  const remote = join(e.dir, "remote");
  const childEnv = { ...process.env, BOTHY_HOME: join(e.dir, "cli"), BOTHY_TOKEN_STORE: "file" };
  delete childEnv.BOTHY_DB;
  const run = (...args) => execFileSync(cli, ["--json", ...args], { env: childEnv, encoding: "utf8" });

  const dbId = JSON.parse(run("init", "--remote", `path:${remote}`, "--app-id", APP)).db_id;
  run("put", "missions", "from_cli", '{"goal": "dock"}');
  run("sync");

  // e.open() tracks the database, so cleanup closes it before deleting the folder (Windows
  // cannot delete open files, and t.after hooks run in the order they were registered).
  const db = await e.open("node");
  assert.equal(await db.initOrJoin(), dbId);
  assert.deepStrictEqual(await db.collection("missions").get("from_cli"), { goal: "dock" });
  await db.collection("telemetry_logs").put("t1", { temp_c: 41.5 });
  await db.collection("missions").patch("from_cli", { status: "done" });
  await db.sync();

  run("sync");
  assert.deepStrictEqual(JSON.parse(run("get", "telemetry_logs", "t1")), { temp_c: 41.5 });
  assert.deepStrictEqual(JSON.parse(run("get", "missions", "from_cli")), { goal: "dock", status: "done" });
});
