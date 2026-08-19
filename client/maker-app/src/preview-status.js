// How ONE preview row is presented — the pure half, so it can be tested.
//
// preview.js dereferences `window.MakerUI` at import time (`:10`) and maker-shared.js does the
// same (`:7-8`), so neither can be loaded by `node --test`. This file is plain functions over
// plain values, the shape src/session.js established. See client/maker-app/test/preview-status
// .test.js, run by `bin/ci test-app`.
//
// THE BUG THIS FILE EXISTS FOR. A preview card made FOUR independent decisions from one server
// row, each keyed on a DIFFERENT field, so no two of them had to agree:
//
//   badge class    ← `status`, through a four-entry map with no `unsupported` key
//   thumbnail      ← `url` FIRST, status only as a fallback
//   red error block← `p.error` truthiness — never status at all
//   click target   ← `url` — never status at all
//
// Six of the nine reachable combinations disagreed. The worst is the one the server works hardest
// to prevent: on a CONTENT REPOINT `Maker::Previews::Enqueue` sets `status = "pending"` but does
// NOT purge the attachment (`enqueue.rb:100-115` — the row "still holds the PREVIOUS render until
// the job finishes"), and `V1::Maker::PreviewsController:114` mints `url` from
// `asset.attached?`, NOT from `ready`. So `pending` arrives WITH a url, and the card painted the
// PREVIOUS CONTENT'S picture under a "pending" badge and zoomed it on click — precisely the lie
// CLAUDE.md §14h says `pending` exists to replace with an honest spinner. The web layer already
// honours it (`_thumb.html.erb:9` and `thumbs_controller.js:39` each require `ready` AND a url);
// this consumer was the only one that did not.
//
// Others that were reachable: `pending` after a prior failure showed a RED error from a render
// nobody was attempting (`Enqueue` never clears `error`); `processing` mid-retry showed a busy
// badge, a spinner and a red error at once (`generate_preview_job.rb:126-128` deliberately leaves
// the status alone and appends "(retrying)"); `failed` holding a stale artifact ZOOMED instead of
// offering the retry that is the whole point of that state; and `unsupported` fell off the badge
// map to `cls = ""` while `generate_preview_job.rb:124` always sets an error, giving a failure
// presentation to what `preview.rb:16-18` calls "a deliberate, non-error terminal state".
//
// THIS TABLE IS FOR `kind: "thumbnail"`, ON A PAGE THAT DOES NOT POLL. If a `geometry` row is ever
// added it must be a SEPARATE table, never this one widened. §14h: geometry's consumer
// (`FilesController#geometry`) answers `pending` WITHOUT reading the row's status, because its
// client polls and an older build's payload is wrong rather than merely old — the same mechanism
// with the opposite right answer. Do not unify them.

/**
 * The five statuses `Maker::Preview::STATUSES` declares (`app/models/maker/preview.rb:19`).
 *
 * A `Map`, not an object literal, so a status of `"constructor"` or `"__proto__"` misses instead
 * of resolving through Object.prototype. `url` is the value that has ALREADY been through
 * `safeHref`, so a rejected URL reaches here as the empty string and reads as "no artifact".
 *
 *   badgeClass  a `.prev-status` modifier — "" is the base (neutral grey) look.
 *   thumb       "image" | "spinner" | "icon"
 *   tone        "err" (red) | "muted" — how `error` is rendered, never WHETHER.
 *   action      "zoom" | "retry" | "inert"
 */
const ROWS = new Map([
  // THE ONE RULE — `ready` AND a url — is what gives both halves of §14h's "a content repoint
  // blanks the artifact; a renderer rebuild does not" the right answer here, with no second copy
  // of that distinction on this side. A rebuild leaves the row `ready`, so the correct picture
  // stays up; a repoint flips it to `pending`, so the stale picture goes. Do NOT instead "fix"
  // this server-side by purging the asset on a repoint: that blanks on a REBUILD too, which is
  // the exact regression `enqueue.rb:107-109` records as having shipped and read as "generating
  // preview is broken". `ready` with no artifact is degenerate rather than terminal, so it keeps
  // a retry rather than becoming a dead card.
  ["ready", (url) => ({
    badgeClass: "ok",
    thumb: url ? "image" : "icon",
    tone: "muted",
    action: url ? "zoom" : "retry",
  })],

  // A SPINNER WHETHER OR NOT THERE IS A URL — see this file's header. The url on a `pending` row
  // is the PREVIOUS content's render.
  //
  // `retry` is not decoration. `pending` and `processing` are ASSERTIONS THAT A JOB EXISTS, and
  // `Maker::Previews::Enqueue::STALLED_AFTER` is the only thing in the system that ever verifies
  // one — nothing here models time at all, so this click is this surface's only recovery from a
  // lost job. Do NOT add a client-side stall clock to decide it: that is a second, drifting copy
  // of `STALLED_AFTER`, and the server already self-heals within its window.
  ["pending", () => ({ badgeClass: "busy", thumb: "spinner", tone: "muted", action: "retry" })],

  // `generate_preview_job.rb:126-128` leaves the status at `processing` on a transient fetch
  // error and appends "(retrying)" to `error`. That is a progress note, not a failure, so it is
  // shown MUTED beside the spinner rather than in red next to it.
  ["processing", () => ({ badgeClass: "busy", thumb: "spinner", tone: "muted", action: "retry" })],

  // NEVER "image", EVEN WITH A URL. A `failed` row that carries an artifact carries the artifact
  // of some EARLIER content (the repoint left it in place, then the render of the new bytes
  // failed), so showing it labels old bytes as this file — and zooming it would take the retry
  // away in the one state that exists to be retried.
  ["failed", () => ({ badgeClass: "err", thumb: "icon", tone: "err", action: "retry" })],

  // A DELIBERATE, NON-ERROR TERMINAL STATE (`preview.rb:16-18`). Neutral badge, static icon, and
  // inert — re-enqueueing can only land back on `unsupported`, and the old card fired
  // `enqueueFor(node_id)` with `filename` undefined, so it could not even reach a different
  // renderer. The error string is still RENDERED, just muted: it is the only thing that names the
  // cause ("no preview renderer for .xyz"), so dropping it would hide information rather than
  // demote it.
  ["unsupported", () => ({ badgeClass: "", thumb: "icon", tone: "muted", action: "inert" })],
]);

/**
 * A status this build does not know — `preview.rb:19` is the source of truth and can gain a sixth
 * entry without anything on this side going red. Nothing here can observe that (a cross-language
 * guard is out of this file's reach), so the honest mitigation is that the fallback is EXPLICIT
 * and safe rather than an accidental `|| ""`:
 *
 *   badgeClass "busy"    — deliberately NOT `unsupported`'s neutral "": this client cannot claim
 *                          an unrecognised state is settled. Not "err" either, which would assert
 *                          a failure that may not exist.
 *   thumb      "icon"    — never a spinner. This page does not poll, so a spinner on a state we
 *                          cannot resolve spins until the user hits Refresh.
 *   action     "retry"   — harmless and recoverable; `inert` would leave no way out.
 */
const UNKNOWN = () => ({ badgeClass: "busy", thumb: "icon", tone: "muted", action: "retry" });

/**
 * Every per-card decision for one preview row, from one lookup.
 *
 * TOTAL by construction: every field is defined for every input, including a status this build
 * has never heard of. The four call sites in preview.js read fields off this and therefore cannot
 * disagree with each other the way four separate re-derivations did.
 *
 * @param {string} status the row's `status` — one of `Maker::Preview::STATUSES`, or anything.
 * @param {string} error  the row's `error`, which several statuses carry WITHOUT being failures.
 * @param {string} url    the artifact URL AFTER `safeHref` — "" when there is nothing to show.
 * @returns {{badgeClass: string, thumb: string, reason: string, reasonClass: string, action: string}}
 */
export function previewPresentation(status, error, url) {
  const row = (ROWS.get(status) || UNKNOWN)(url);
  // `reason` is the error string as PRESENTED. `reasonClass` decides the tone only — whether it
  // is shown at all is decided here, by whether there is one, on every status alike.
  const reason = error ? String(error) : "";
  return {
    badgeClass: row.badgeClass,
    thumb: row.thumb,
    reason,
    // `.prev-error` carries the size/margin either way; only the leading tone class moves.
    reasonClass: reason ? `${row.tone} prev-error` : "",
    action: row.action,
  };
}
