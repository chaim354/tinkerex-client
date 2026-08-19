// The desktop app's account-identity rules. Run by `bin/ci test-app` (config/ci.rb) and by the
// `test-app-js` job in .github/workflows/ci.yml.
//
// THIS IS THE FIRST JS TEST IN THE REPO. Before it, client/maker-app had no `test` script, no test
// files, and no gate — the Rust `mod tests` in src-tauri/src/lib.rs covers Prefs/creds_key/atomic
// writes and touches none of this. CLAUDE.md §14c records what a guard in a file no gate runs is
// worth (the sidecar catalog's pytest, which nothing in bin/ci executed), so the gate landed with
// the file.
//
// Everything here is a pure function or takes an injected `invoke`; main.js itself dereferences
// window.__TAURI__ and document at import time and cannot be loaded here. src/session.js exists
// for that reason.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  accountArgs,
  accountAfter,
  fetchConflicts,
  signedIn,
} from "../src/session.js";

// index.html:36,40 — the values the sign-in form carries on every launch, and therefore what the
// old fallback aimed 64 catalog commands at.
const DEV_DEFAULTS = { serverUrl: "http://localhost:3001", email: "maker@example.com" };
const REAL = { serverUrl: "https://sync.example.org", email: "owner@example.org" };

// ---------------------------------------------------------------------------------------------
// 1. With no signed-in account, args() yields no serverUrl/email — never the form's values.
// ---------------------------------------------------------------------------------------------

test("accountArgs answers null when signed out, whatever the form says", () => {
  assert.equal(accountArgs(null, DEV_DEFAULTS), null);
  assert.equal(accountArgs(undefined, DEV_DEFAULTS), null);

  // The property the call sites depend on: spreading it is empty, so `{ ...args(), id }` carries
  // no account at all and the Tauri bridge rejects the payload inside their existing catch.
  const spread = { ...accountArgs(null, DEV_DEFAULTS), id: 7 };
  assert.deepEqual(spread, { id: 7 });
  assert.equal("serverUrl" in spread, false);
  assert.equal("email" in spread, false);
});

test("accountArgs answers the SIGNED-IN account, not the form, when the two disagree", () => {
  // The other half of the same rule, and the one that catches a fallback restored as a merge or
  // a default rather than as a `??`. The form here says something different on purpose.
  assert.deepEqual(accountArgs(REAL, DEV_DEFAULTS), REAL);
});

test("signedIn is the predicate the surfaces render their empty state from", () => {
  assert.equal(signedIn(null), false);
  assert.equal(signedIn(undefined), false);
  assert.equal(signedIn(REAL), true);
});

// ---------------------------------------------------------------------------------------------
// 2. An unmount does NOT clear a signed-in account.
// ---------------------------------------------------------------------------------------------

test("unmounting leaves the signed-in account exactly as it was", () => {
  const after = accountAfter(REAL, { type: "unmounted" });
  assert.deepEqual(after, REAL);
  assert.deepEqual(accountArgs(after, DEV_DEFAULTS), REAL);

  // Repeated unmounts (the button is clickable whenever a mount exists) stay a no-op.
  assert.deepEqual(accountAfter(accountAfter(REAL, { type: "unmounted" }), { type: "unmounted" }), REAL);
});

test("a sign-in replaces the account — the only thing that ever changes it", () => {
  const first = accountAfter(null, { type: "signed-in", ...REAL });
  assert.deepEqual(first, REAL);

  const switched = accountAfter(first, { type: "signed-in", ...DEV_DEFAULTS });
  assert.deepEqual(switched, DEV_DEFAULTS);
});

test("an unrecognised event leaves the account alone rather than blanking it", () => {
  // A failed sign-in sends no event at all; this covers the direction anyway, because the wrong
  // default here is silent — it would blank a working account with nothing to show for it.
  assert.deepEqual(accountAfter(REAL, { type: "whatever" }), REAL);
  assert.deepEqual(accountAfter(REAL, undefined), REAL);
});

// ---------------------------------------------------------------------------------------------
// 3. pollConflicts issues no invoke when there is no signed-in account.
// ---------------------------------------------------------------------------------------------

// A COUNTING double, not a throwing one. fetchConflicts' `catch { return null; }` swallows a
// raiser whole, so a double that throws would let the guard's deletion pass — CLAUDE.md §14c's
// recorded precedent (Registry.manifests rescues StandardError, so a raising stub proved nothing).
const countingInvoke = (answer = []) => {
  const calls = [];
  const invoke = async (cmd, args) => {
    calls.push({ cmd, args });
    return answer;
  };
  invoke.calls = calls;
  return invoke;
};

test("fetchConflicts issues NO invoke when signed out", async () => {
  const invoke = countingInvoke();
  const result = await fetchConflicts({ invoke, account: null });

  assert.equal(invoke.calls.length, 0, "signed out, the 5s conflict poll must not call the bridge");
  assert.equal(result, null);
});

test("fetchConflicts asks once, with the signed-in account, when signed in", async () => {
  const invoke = countingInvoke([{ node_id: "n1" }]);
  const result = await fetchConflicts({ invoke, account: REAL });

  assert.equal(invoke.calls.length, 1);
  assert.equal(invoke.calls[0].cmd, "conflicts");
  assert.deepEqual(invoke.calls[0].args, REAL);
  assert.deepEqual(result, [{ node_id: "n1" }]);
});

test("fetchConflicts answers null on a bridge error, so the caller leaves the queue alone", async () => {
  const invoke = async () => {
    throw new Error("daemon not up");
  };
  assert.equal(await fetchConflicts({ invoke, account: REAL }), null);
});

test("fetchConflicts normalises a null answer to an empty list", async () => {
  // Distinct from the null above: the server answered, there is just nothing queued. The caller
  // branches on `if (!list) return;`, so conflating the two would make an empty answer look like
  // an error — harmless today, and a trap for anyone who later acts on the distinction.
  const invoke = countingInvoke(null);
  assert.deepEqual(await fetchConflicts({ invoke, account: REAL }), []);
});
