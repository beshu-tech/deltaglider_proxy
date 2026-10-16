import assert from "node:assert/strict";
import { test } from "vitest";
import {
  cloneRule,
  copyRuleName,
  sameScopeRule,
} from "../components/ruleClone";
import { emptyRule as emptyReplication } from "../components/replicationPayload";
import { emptyRule as emptyLifecycle } from "../components/lifecyclePayload";
import { ruleUid } from "../ruleUid";

test("copyRuleName takes the first free name", () => {
  assert.equal(copyRuleName("nightly", []), "nightly-copy");
  assert.equal(
    copyRuleName("nightly", [{ name: "nightly-copy" }]),
    "nightly-copy-2",
  );
  assert.equal(
    copyRuleName("nightly", [
      { name: "nightly-copy" },
      { name: "nightly-copy-2" },
    ]),
    "nightly-copy-3",
  );
});

test("cloneRule copies deeply, renames, and gets its own uid", () => {
  const src = {
    ...emptyReplication([]),
    name: "a",
    source: { bucket: "s", prefix: "p/" },
  };
  const copy = cloneRule(src, [src]);
  assert.equal(copy.name, "a-copy");
  assert.deepEqual(copy.source, src.source);
  assert.notEqual(copy.source, src.source, "no shared nested object");
  assert.notEqual(ruleUid(copy), ruleUid(src));
});

test("sameScopeRule names the rule that acts on the same objects", () => {
  const a = {
    ...emptyReplication([]),
    name: "a",
    source: { bucket: "s", prefix: "" },
    destination: { bucket: "d", prefix: "" },
  };
  const copy = cloneRule(a, [a]);
  assert.equal(sameScopeRule("replication", [a, copy], 1), "a");
  const moved = { ...copy, destination: { bucket: "other", prefix: "" } };
  assert.equal(sameScopeRule("replication", [a, moved], 1), null);
  const filtered = { ...copy, exclude_globs: ["*.tmp"] };
  assert.equal(
    sameScopeRule("replication", [a, filtered], 1),
    null,
    "other filters: other objects",
  );
  const blank = emptyReplication([]);
  assert.equal(
    sameScopeRule("replication", [blank, emptyReplication([])], 1),
    null,
    "no buckets yet",
  );

  const l = { ...emptyLifecycle([]), name: "x", bucket: "b", prefix: "logs/" };
  const lc = cloneRule(l, [l]);
  assert.equal(sameScopeRule("lifecycle", [l, lc], 1), "x");
  assert.equal(
    sameScopeRule("lifecycle", [l, { ...lc, bucket: "c" }], 1),
    null,
  );
});
