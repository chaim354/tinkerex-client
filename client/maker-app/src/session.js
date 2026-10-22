// Which ACCOUNT a catalog command is aimed at — the pure half, so it can be tested.
//
// main.js touches `window.__TAURI__` and `document` at import time and can therefore never be
// loaded by a test runner. Everything in this file is a plain function over plain values, with
// `invoke` injected where a call is unavoidable. See client/maker-app/test/session.test.js, run
// with `node --test client/maker-app/test` (no CI job runs it; its header says why).
//
// THE BUG THIS FILE EXISTS FOR. main.js pinned the account at sign-in and CLEARED IT ON UNMOUNT,
// falling back to the live sign-in form — whose values were then index.html's dev defaults,
// http://localhost:3001 and maker@example.com. But the ~60 `maker_*` catalog commands never touch
// the mount, and the catalog is usable while unmounted, so scoping the only record of *which
// account* to the mount's lifetime retargeted every one of them at those defaults — twice over,
// since the account is also null on every fresh launch before the first sign-in. Most commands
// resolve a token keyed on (serverUrl, email) and so merely error, misleadingly; on a developer
// box, where localhost:3001 / maker@example.com is a real enrolled account, they SUCCEED,
// including the writes. `set_prefs` takes no token at all and writes under that key on any box.
//
// Identity is not the mount's to own: the device token file and the account directory outlive
// both the mount and the process.

/**
 * The `{ serverUrl, email }` every catalog command carries — or NULL when nobody is signed in.
 *
 * `formValues` is accepted and deliberately never read. It is the thing this function must not
 * return, and taking it as a parameter is what puts that decision inside a tested function:
 * restoring the old fallback is then a one-token edit HERE, which a test can catch, rather than
 * an edit in main.js, which no test can see.
 *
 * RETURNS NULL RATHER THAN THROWING, and that is load-bearing. The 64 call sites either spread
 * this (`{ ...args(), id }` → `{ id }`) or hand it straight to `invoke`; both make the Tauri
 * bridge reject the payload inside the try/catch each site already has. Three sites in
 * library.js (:56, :144, :240) call it OUTSIDE their try block, so a throw would be uncaught.
 */
export const accountArgs = (account, formValues) =>
  account ? { serverUrl: account.serverUrl, email: account.email } : null;

/** Has a device login succeeded in this process? Callers use it to render a signed-out state. */
export const signedIn = (account) => account != null;

/** What a surface shows instead of a raw Tauri deserialization error when signed out. */
export const SIGNED_OUT_HINT = "Sign in to browse your library.";

/**
 * The signed-in account after a lifecycle event — the ONE place its lifetime is written down.
 *
 * It is a function rather than three assignments in main.js for exactly one reason: the
 * `unmounted` arm must be a NO-OP, and a no-op living inside a click handler is a line anyone
 * can "tidy" back to `account = null` with nothing going red. Here it is a case a test holds.
 *
 *   "signed-in"  a successful device login. REPLACES whatever was there — this is the explicit
 *                account switch, and the only way back from a sign-out.
 *   "unmounted"  the drive stopped. Identity untouched; unmounting is not signing out.
 *   "signed-out" the SERVER refused this device's token (tinkerex#472) and the backend has
 *                forgotten the record. The account really is gone, so this is the one event
 *                that clears it: every catalog command stops asking until the next sign-in.
 *
 * A FAILED sign-in sends no event, so it leaves the working account alone rather than blanking
 * it. (The mount PHASE — mounting/mounted/unmounted — is a separate concern that still lives as
 * a parsed status string in main.js; it is not this.)
 */
export function accountAfter(account, event) {
  switch (event && event.type) {
    case "signed-in":
      return { serverUrl: event.serverUrl, email: event.email };
    case "unmounted":
      return account; // NOT null — see this file's header
    case "signed-out":
      return null;
    default:
      return account;
  }
}

/**
 * One poll of the conflict queue. `invoke` is injected so the guard below is testable.
 *
 * Answers an ARRAY when the server answered, and NULL for "no answer, leave the queue alone" —
 * either signed out (in which case NO invoke is issued at all) or a transient bridge error. The
 * caller polls every 5 s, so there is nothing to retry here.
 *
 * This is the path that made the bug invisible: it runs from boot with no user action and
 * swallows its own errors, so a signed-out launch quietly asked localhost:3001 for conflicts
 * every five seconds forever.
 */
export async function fetchConflicts({ invoke, account }) {
  const a = accountArgs(account);
  if (!a) return null; // signed out — nothing to ask, and no invoke issued
  try {
    return (await invoke("conflicts", a)) || [];
  } catch {
    return null; // daemon not up yet, etc.
  }
}

// ---- signed out by the server (tinkerex#472) ------------------------------------------------
//
// Since tinkerex#466 the maker app can sign a device out (Settings → Devices, a password change,
// DEVICE_IDLE_DAYS of idleness), and the sync server will too (tinkerex#473). The backend turns
// the server's refusal into ONE answer, whichever of the ~60 commands met it — SIGNED_OUT in
// src-tauri/src/lib.rs, which starts with SIGNED_OUT_PREFIX — and forgets the dead record, so the
// next sign-in really signs in. test/signed-out.test.js holds the two halves together.

/** How every "signed out" answer from the backend begins (lib.rs `SIGNED_OUT`). */
export const SIGNED_OUT_PREFIX = "Signed out:";

/** The backend's answer once the record is gone (lib.rs `NOT_SIGNED_IN`). */
export const NOT_SIGNED_IN = "this device isn't signed in yet";

/** What the signed-out notice and toasts say. */
export const SIGNED_OUT_NOTICE =
  "This device was signed out of Tinkerex: its sign-in was revoked or has expired. " +
  "Sign in again to keep syncing. Nothing on this device was deleted.";

/**
 * Did the backend just say this device is signed out? Only its two signed-out answers count.
 * Offline, a 5xx, a proxy's 401 and a wrong password are ordinary errors: signing a device out
 * for any of them would be the opposite bug.
 */
export const isSignedOutError = (e) => {
  const text = e == null ? "" : String(e);
  return text.startsWith(SIGNED_OUT_PREFIX) || text === NOT_SIGNED_IN;
};

/**
 * Wrap the Tauri `invoke` so a signed-out answer from ANY command reaches `onSignedOut`, whichever
 * surface made the call — including the background polls that swallow their own errors. The
 * rejection is re-thrown unchanged, so each caller's own catch still runs as before.
 */
export function guardInvoke(invoke, onSignedOut) {
  return async (cmd, args) => {
    try {
      return await invoke(cmd, args);
    } catch (e) {
      if (isSignedOutError(e)) onSignedOut(String(e));
      throw e;
    }
  };
}

/**
 * The status pill for a `sync_status` answer — `{ state, mountpoint }`, state one of "mounted",
 * "signed_out", "unmounted" — or the transient "mounting" the sign-in button sets. "signed_out"
 * is still mounted (the drive serves local files; edits queue) but nothing syncs, and the pill
 * used to say "mounted at …" for it.
 */
export function pillFor(status) {
  if (status === "mounting") return { className: "pill busy", label: "signing in…", unmount: false };
  switch (status && status.state) {
    case "mounted":
      return { className: "pill on", label: `mounted at ${status.mountpoint}`, unmount: true };
    case "signed_out":
      return { className: "pill warn", label: "signed out — sync paused", unmount: true };
    default:
      return { className: "pill", label: "unmounted", unmount: false };
  }
}
