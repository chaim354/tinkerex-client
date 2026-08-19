// How a preview card presents one server row. Run by `bin/ci test-app` (config/ci.rb) and by the
// `test-app-js` job in .github/workflows/ci.yml — the gate globs `test/**/*.test.js` in Ruby, so
// this file needed no edit there.
//
// src/preview.js and src/maker-shared.js both dereference `window.MakerUI` at module top level and
// so can never be imported here; src/preview-status.js exists for that reason, the same split
// src/session.js made. NOTHING IN THIS REPO COULD OBSERVE ANY OF THESE CASES BEFORE — the desktop
// app's preview surface had no test of any kind.

import { test } from "node:test";
import assert from "node:assert/strict";

import { previewPresentation } from "../src/preview-status.js";

// app/models/maker/preview.rb:19 — the source of truth. The table's five rows are these.
const STATUSES = ["pending", "processing", "ready", "failed", "unsupported"];

const URL = "http://127.0.0.1:1420/blob/abc";

// ---------------------------------------------------------------------------------------------
// 1. ready — the only status that may show the artifact, and the only one that zooms.
// ---------------------------------------------------------------------------------------------

test("ready with a url shows the image and zooms", () => {
  const v = previewPresentation("ready", null, URL);
  assert.equal(v.badgeClass, "ok");
  assert.equal(v.thumb, "image");
  assert.equal(v.action, "zoom");
  assert.equal(v.reason, "");
  assert.equal(v.reasonClass, "");
});

test("ready with NO artifact is degenerate, not terminal — icon, and a retry rather than a dead card", () => {
  const v = previewPresentation("ready", null, "");
  assert.equal(v.thumb, "icon");
  assert.equal(v.action, "retry");
});

// ---------------------------------------------------------------------------------------------
// 2. THE §14h CASE. Enqueue sets `pending` on a content repoint and does NOT purge the asset
//    (enqueue.rb:100-115), and V1::Maker::PreviewsController:114 mints `url` from
//    `asset.attached?` — never from `ready`. So `pending` ARRIVES WITH A URL, and it is the
//    PREVIOUS content's picture. The old card painted it and zoomed it.
// ---------------------------------------------------------------------------------------------

test("pending WITH a url still shows a spinner — never the previous content's image", () => {
  const v = previewPresentation("pending", null, URL);
  assert.equal(v.thumb, "spinner", "a repointed row's url is the OLD content — §14h");
  assert.notEqual(v.thumb, "image");
  assert.equal(v.badgeClass, "busy");
});

test("pending WITH a url offers retry, NOT zoom", () => {
  // Zooming would open the superseded artifact in a new window, presented as this file.
  const v = previewPresentation("pending", null, URL);
  assert.equal(v.action, "retry");
  assert.notEqual(v.action, "zoom");
});

test("pending with a STALE error is not a failure — muted, never red", () => {
  // `Enqueue` never clears `error`, so a row re-enqueued after a failure carries the old message
  // while nothing is failing. Kept visible; only the tone moves.
  const v = previewPresentation("pending", "RuntimeError: boom", URL);
  assert.equal(v.reason, "RuntimeError: boom");
  assert.equal(v.reasonClass, "muted prev-error");
  assert.ok(!v.reasonClass.includes("err "), "a stale message must not paint the card red");
  assert.equal(v.badgeClass, "busy");
});

// ---------------------------------------------------------------------------------------------
// 3. processing — generate_preview_job.rb:126-128 leaves the status alone and appends
//    "(retrying)". A progress note, not a failure.
// ---------------------------------------------------------------------------------------------

test("processing mid-retry is a spinner with a muted note, not a spinner beside a red error", () => {
  const v = previewPresentation(
    "processing",
    "Maker::FileBytes::FetchError: 503 (retrying)",
    ""
  );
  assert.equal(v.badgeClass, "busy");
  assert.equal(v.thumb, "spinner");
  assert.equal(v.reasonClass, "muted prev-error");
  assert.equal(v.action, "retry", "the only recovery from a lost job — no stall clock lives here");
});

// ---------------------------------------------------------------------------------------------
// 4. failed — the one state that exists to be retried, so a stale artifact must not take the
//    retry away by turning the card into a zoom target.
// ---------------------------------------------------------------------------------------------

test("failed WITH a stale url retries — it does not zoom, and does not show the image", () => {
  const v = previewPresentation("failed", "IOError: no bytes", URL);
  assert.equal(v.badgeClass, "err");
  assert.equal(v.thumb, "icon");
  assert.equal(v.action, "retry");
  assert.notEqual(v.action, "zoom");
  assert.equal(v.reasonClass, "err prev-error", "a real failure IS red");
});

// ---------------------------------------------------------------------------------------------
// 5. unsupported — preview.rb:16-18 "a deliberate, non-error terminal state". It fell off the old
//    four-entry badge map entirely, while generate_preview_job.rb:124 always sets an error.
// ---------------------------------------------------------------------------------------------

// SPLIT INTO TWO TESTS ON PURPOSE, not tidied back into one. node:test abandons a test at its
// FIRST failed assertion, so a single test proves only that whichever assertion runs first carries
// weight — and deleting this row from the table breaks BOTH the badge and the click. Two tests is
// how both are shown to fail. §15: assert the narrowest true thing, then mutate to prove each
// assertion carries weight.
test("unsupported gets a neutral badge and a static icon", () => {
  const v = previewPresentation("unsupported", "no preview renderer for .xyz", "");
  assert.equal(v.badgeClass, "", "neutral — .prev-status's base look, not .err");
  assert.equal(v.thumb, "icon");
});

test("unsupported is inert — there is nothing a re-render could change", () => {
  const v = previewPresentation("unsupported", "no preview renderer for .xyz", "");
  assert.equal(v.action, "inert", "re-enqueueing can only land back on unsupported");
});

test("unsupported still RENDERS its reason — it is the only thing naming the cause", () => {
  const v = previewPresentation("unsupported", "no preview renderer for .xyz", "");
  assert.equal(v.reason, "no preview renderer for .xyz");
  assert.equal(v.reasonClass, "muted prev-error", "demoted, not dropped");
});

// ---------------------------------------------------------------------------------------------
// 6. The explicit unknown default. preview.rb:19 can gain a sixth status with nothing on this
//    side going red, so the fallback must be a decision rather than an accidental `|| ""`.
// ---------------------------------------------------------------------------------------------

test("an unrecognised status falls to an explicit default, distinct from unsupported's", () => {
  const v = previewPresentation("quarantined", null, "");
  assert.equal(v.thumb, "icon", "never a spinner — this page does not poll");
  assert.equal(v.action, "retry", "recoverable; inert would leave no way out");
  assert.notEqual(v.badgeClass, "", "must not present as unsupported's settled neutral state");
  assert.notEqual(v.action, "inert");
});

test("a status naming an Object.prototype member misses the table rather than resolving through it", () => {
  for (const evil of ["constructor", "__proto__", "toString"]) {
    const v = previewPresentation(evil, null, URL);
    assert.equal(typeof v.badgeClass, "string", `${evil} must not resolve to a function`);
    assert.equal(v.thumb, "icon");
    assert.equal(v.action, "retry");
  }
});

// ---------------------------------------------------------------------------------------------
// 7. TOTALITY. Every field defined for every declared status, with and without a url and an
//    error — the property that makes four field reads safe where four re-derivations were not.
// ---------------------------------------------------------------------------------------------

test("every declared status answers every field, in all four url/error combinations", () => {
  for (const status of STATUSES) {
    for (const url of ["", URL]) {
      for (const error of [null, "boom"]) {
        const v = previewPresentation(status, error, url);
        const where = `${status} url=${!!url} error=${!!error}`;

        assert.equal(typeof v.badgeClass, "string", `${where}: badgeClass`);
        assert.ok(["", "ok", "busy", "err"].includes(v.badgeClass), `${where}: badgeClass value`);
        assert.ok(["image", "spinner", "icon"].includes(v.thumb), `${where}: thumb`);
        assert.ok(["zoom", "retry", "inert"].includes(v.action), `${where}: action`);
        assert.equal(typeof v.reason, "string", `${where}: reason`);
        assert.equal(typeof v.reasonClass, "string", `${where}: reasonClass`);

        // The two coupled rules, over the whole table rather than one row at a time.
        assert.equal(v.thumb === "image", status === "ready" && !!url, `${where}: image iff ready+url`);
        assert.equal(v.action === "zoom", status === "ready" && !!url, `${where}: zoom iff ready+url`);
        assert.equal(!!v.reason, !!error, `${where}: an error is always shown, never dropped`);
        assert.equal(v.reasonClass === "", !error, `${where}: reasonClass tracks reason`);
      }
    }
  }
});
