// The desktop app's signed-out state (tinkerex#472). Run with `node --test client/maker-app/test`
// from the repo root; like session.test.js, NO CI JOB RUNS IT (that file's header says why).
//
// Since tinkerex#466 the maker app can sign a device out, and the sync server will too
// (tinkerex#473). Before this, a refused token surfaced as a raw `list items failed: server 401:
// {"error":"unauthorized"}` toast, the status pill kept saying "mounted", and nothing offered a way
// back. The backend now answers one sentence starting `Signed out:` and forgets the dead record;
// these are the frontend's half: recognise that answer from ANY command, drop the account, say so
// with a "Sign in again" action, and never mistake an ordinary failure for it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  accountAfter,
  accountArgs,
  fetchConflicts,
  guardInvoke,
  isSignedOutError,
  pillFor,
  NOT_SIGNED_IN,
  SIGNED_OUT_PREFIX,
} from "../src/session.js";

const REAL = { serverUrl: "https://tinkerex.example.com", email: "owner@example.org" };

// Read as text: the Rust constants are the other half of this contract.
const LIB_RS = readFileSync(new URL("../src-tauri/src/lib.rs", import.meta.url), "utf8");
const rustConst = (name) => {
  const m = LIB_RS.match(new RegExp(`const ${name}: &str =\\s*"((?:[^"\\\\]|\\\\.)*)"`, "s"));
  assert.ok(m, `lib.rs defines ${name}`);
  // A Rust `\` line continuation drops the newline and the next line's leading whitespace.
  return m[1].replace(/\\\n\s*/g, "");
};
const SIGNED_OUT = rustConst("SIGNED_OUT");

// ---------------------------------------------------------------------------------------------
// 1. Which answers mean "signed out" — and which never do.
// ---------------------------------------------------------------------------------------------

test("the backend's SIGNED_OUT sentence is what the frontend recognises", () => {
  assert.ok(SIGNED_OUT.startsWith(SIGNED_OUT_PREFIX), SIGNED_OUT);
  assert.equal(isSignedOutError(SIGNED_OUT), true);
  // Tauri rejects with the bare string; an Error wrapping it reads the same.
  assert.equal(isSignedOutError(new Error(SIGNED_OUT).message), true);
});

test("a record already forgotten (NOT_SIGNED_IN) is the same signed-out state", () => {
  assert.equal(NOT_SIGNED_IN, rustConst("NOT_SIGNED_IN"));
  assert.equal(isSignedOutError(NOT_SIGNED_IN), true);
});

test("offline, a 5xx, a proxy and a wrong password never sign the device out", () => {
  for (const e of [
    "list items failed: http: error sending request for url (https://tinkerex.example.com/v1/maker/items)",
    'browse failed: server 503: {"error":"unavailable"}',
    "list tags failed: server 401: <html>401 Authorization Required</html>",
    "That email and password did not sign in to this Tinkerex server.",
    "already mounted — unmount first",
    "",
    null,
    undefined,
  ]) {
    assert.equal(isSignedOutError(e), false, String(e));
  }
});

// ---------------------------------------------------------------------------------------------
// 2. Every command goes through one guard, so no surface can miss it.
// ---------------------------------------------------------------------------------------------

const recorder = () => {
  const calls = [];
  const fn = (msg) => calls.push(msg);
  fn.calls = calls;
  return fn;
};

test("a signed-out answer from any command reaches the app once, and the caller still sees it", async () => {
  const onSignedOut = recorder();
  const invoke = guardInvoke(async () => {
    throw SIGNED_OUT;
  }, onSignedOut);

  await assert.rejects(invoke("maker_list_items", REAL), (e) => e === SIGNED_OUT);
  assert.deepEqual(onSignedOut.calls, [SIGNED_OUT]);
});

test("an ordinary failure passes through the guard untouched", async () => {
  const onSignedOut = recorder();
  const boom = 'browse failed: server 503: {"error":"unavailable"}';
  const invoke = guardInvoke(async () => {
    throw boom;
  }, onSignedOut);
  await assert.rejects(invoke("browse", REAL), (e) => e === boom);
  assert.equal(onSignedOut.calls.length, 0);
});

test("a success passes through the guard untouched", async () => {
  const onSignedOut = recorder();
  const seen = [];
  const invoke = guardInvoke(async (cmd, args) => {
    seen.push({ cmd, args });
    return [1, 2];
  }, onSignedOut);
  assert.deepEqual(await invoke("maker_list_tags", REAL), [1, 2]);
  assert.deepEqual(seen, [{ cmd: "maker_list_tags", args: REAL }]);
  assert.equal(onSignedOut.calls.length, 0);
});

test("the 5 s conflict poll, which swallows its errors, still trips the guard", async () => {
  // fetchConflicts' own catch returns null; without the guard around `invoke` a sign-out first
  // seen by this background poll would be swallowed with nothing shown.
  const onSignedOut = recorder();
  const invoke = guardInvoke(async () => {
    throw SIGNED_OUT;
  }, onSignedOut);
  assert.equal(await fetchConflicts({ invoke, account: REAL }), null);
  assert.equal(onSignedOut.calls.length, 1);
});

// ---------------------------------------------------------------------------------------------
// 3. Signed out, the account is gone — every catalog command stops asking.
// ---------------------------------------------------------------------------------------------

test("signing out clears the account, unlike an unmount", () => {
  assert.equal(accountAfter(REAL, { type: "signed-out" }), null);
  assert.equal(accountArgs(accountAfter(REAL, { type: "signed-out" })), null);
  assert.deepEqual(accountAfter(REAL, { type: "unmounted" }), REAL, "unchanged: unmount is not sign-out");
  // And the next sign-in is the way back.
  assert.deepEqual(accountAfter(null, { type: "signed-in", ...REAL }), REAL);
});

// ---------------------------------------------------------------------------------------------
// 4. The status pill says what is true.
// ---------------------------------------------------------------------------------------------

test("a refused mount shows signed out, never mounted", () => {
  const pill = pillFor({ state: "signed_out", mountpoint: "/Users/ada/Tinkerex" });
  assert.equal(pill.className, "pill warn");
  assert.match(pill.label, /^signed out/);
  assert.doesNotMatch(pill.label, /^mounted/);
  assert.equal(pill.unmount, true, "the drive is still mounted, so it can still be unmounted");
});

test("the other pill states are unchanged", () => {
  assert.deepEqual(pillFor({ state: "mounted", mountpoint: "/Users/ada/Tinkerex" }), {
    className: "pill on",
    label: "mounted at /Users/ada/Tinkerex",
    unmount: true,
  });
  assert.deepEqual(pillFor({ state: "unmounted", mountpoint: null }), {
    className: "pill",
    label: "unmounted",
    unmount: false,
  });
  assert.deepEqual(pillFor("mounting"), { className: "pill busy", label: "signing in…", unmount: false });
  assert.deepEqual(pillFor(undefined), { className: "pill", label: "unmounted", unmount: false });
});

// ---------------------------------------------------------------------------------------------
// 5. The page carries the signed-out state and its action.
// ---------------------------------------------------------------------------------------------

const INDEX_HTML = readFileSync(new URL("../src/index.html", import.meta.url), "utf8");

test("the sign-in card has a hidden signed-out notice with a Sign in again button", () => {
  const notice = (INDEX_HTML.match(/<div id="signedOutNotice"[^>]*>[\s\S]*?<\/div>/) || [])[0];
  assert.ok(notice, "index.html has #signedOutNotice");
  assert.match(notice, /class="[^"]*\bhidden\b/, "hidden until the app is signed out");
  assert.match(notice, /role="alert"/);
  assert.match(notice, /<button id="signInAgain"[^>]*>Sign in again<\/button>/);
});
