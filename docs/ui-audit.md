# UI audit — maker-app (Tauri client)

_Audit date: 2026-06-12. Scope: the desktop client frontend at `client/maker-app/src`._

## 1. Current stack & structure

- **Tauri 2** desktop app, **vanilla HTML/CSS/JS** — no framework, no build step
  (`frontendDist: "../src"`, `withGlobalTauri: true`). This is a deliberate, lean
  choice and matches the "no new heavy UI framework" constraint.
- **Everything lives in one file**: `src/index.html` carries the entire UI — markup,
  a `<style>` block (~70 lines of CSS with a small design-token `:root`), and an inline
  `<script type="module">` (~210 lines) that calls Tauri commands via
  `window.__TAURI__.core.invoke`.
- **Dead scaffold**: `src/main.js` and `src/styles.css` are the leftover Tauri "vanilla"
  template (a `greet()` demo wired to `#greet-input`/`#greet-msg`, neither of which exist
  in `index.html`). They are **not referenced** by `index.html` and ship nothing — pure
  noise that misleads anyone opening the folder.
- **Backend command contract** (`src-tauri/src/lib.rs`, must not break):
  `sync_start`, `sync_pair`, `create_pairing`, `sync_stop`, `sync_status`, `platform`,
  `browse`, `file_versions`, `restore_version`, `conflicts`, `resolve_conflict_keep`,
  `resolve_conflict_split`. Arg casing is camelCase (`serverUrl`, `email`, `mountpoint`,
  `code`, `nodeId`, `versionId`).

## 2. Screen-by-screen state

### Connection / mount-setup form
- Server URL, email, mount-point inputs; a two-button "tab" toggle (This is my device /
  Pair with a code); Mount & sync / Unmount buttons; a single toast line.
- Per-OS mount hint is set from `platform()` (drive letter on Windows, directory else).
- **Weak points:** no inline validation (empty email/mount silently passes to Rust and
  fails late); the "tabs" are unlabeled styled buttons with no `role=tab`/`aria-selected`;
  mount/unmount have no busy spinner beyond a text toast; Unmount is always enabled even
  when nothing is mounted.

### Status pill (header)
- Binary `connected`/`disconnected` derived from whether `sync_status()` starts with
  `"mounted"`. **Loses information**: it never shows the mountpoint, never distinguishes
  "mounting in progress" or an error state, and the word "connected" conflates _mounted_
  with _server reachable_.

### Add-another-device
- A button that mints a pairing code into a `.codebox`. **No copy-to-clipboard**, no
  expiry countdown beyond the static "expires in 10 minutes" prose, and errors are dumped
  into the same code box (so an error string renders in the giant letter-spaced code font).

### Files & versions browser
- `browse()` returns a flat list; JS rebuilds the tree by `parent_node_id` and renders
  rows with emoji icons, size, an optional 🔒 lock badge, and a "versions" toggle.
- Version panel lists versions newest-first; non-current ones get a "restore" button.
- **Weak points:**
  - **No empty state** — before first load the list is `hidden`; after a load with zero
    files you get "0 items" and an invisible empty box. No "nothing here yet" guidance.
  - **No real loading state** — just the text "loading…" in a hint span; the list itself
    doesn't show skeletons or a spinner.
  - **Errors replace the hint text** with a raw `String(e)` (e.g. a Rust error string) —
    no styling, easy to miss, persists until the next refresh.
  - **Restore has no confirmation** and on failure overwrites the button label with the
    raw error (the button becomes the error message, permanently broken until re-render).
  - Folders aren't collapsible — a deep tree renders fully expanded every time.
  - Lock badge shows a device name but no tooltip context about what "locked" means.
  - The panel only appears after a manual "Refresh files" or a successful mount.

### Conflict-resolution modal
- A polling loop (`conflicts()` every 5s) opens an overlay with two side cards (server vs
  your other edit), each "Keep this version", plus a "Keep both / split" button.
- **Weak points:**
  - **No way to dismiss/close** the modal without resolving (no X, no Esc, no backdrop
    click) — if the user wants to look at the file first they're trapped.
  - **No queue indicator** when multiple conflicts exist (it shows `list[0]` only).
  - On a keep/split failure the action button's label is overwritten with the raw error
    and left disabled — dead end.
  - The two cards are visually identical, so it's hard to tell which is "incoming".
  - Not keyboard-accessible (no focus trap, no `role="dialog"`/`aria-modal`).

## 3. Cross-cutting weaknesses

- **Visual consistency:** tokens exist but are thin. No spacing scale, no shared button
  variants beyond `.primary`/`.link`, no visible `:focus-visible` style (accessibility +
  keyboard-nav gap), no `:hover` transition. Error toast uses a hard-coded light-pink
  (`#fde8ea`) that doesn't adapt in dark mode.
- **Error handling pattern is uniformly bad:** every catch does
  `something.textContent = String(e)` — raw Rust error strings, often overwriting a
  control's own label and leaving it stuck. No consistent, dismissible error surface.
- **Loading/empty/error states** are ad-hoc text, not first-class UI.
- **Accessibility:** no `aria-live` on the status pill or toasts; tabs/modal lack ARIA
  roles; no focus management; emoji icons have no text alternative; no visible focus ring.
- **Responsiveness:** the conflict modal's `1fr 1fr` side grid doesn't collapse on a
  narrow window.
- **Cross-platform:** only the mount-point hint branches per OS. The active backend
  (WebDAV/FUSE/WinFsp) isn't surfaced, so a Windows user has no in-app cue that a drive
  letter is required.
- **Dead files** (`main.js`, `styles.css`) should be removed or repurposed.

## 4. Prioritized improvements

**P0 — high value, low risk:**
1. Real shared style baseline: spacing scale, focus-visible rings, button variants,
   transitions, dark-mode-safe toast/error colors. Move the CSS into `styles.css`
   (repurpose the dead file) and link it.
2. A consistent, dismissible **error/toast** helper instead of
   `textContent = String(e)`; buttons restore their own label after an action.
3. Proper **loading / empty / error** states for the file browser.
4. **Status indicator** upgrade: three states (unmounted / mounting / mounted-at-path),
   show the mountpoint, `aria-live`.

**P1 — clarity of the core flows:**
5. **Conflict modal**: closable (X/Esc/backdrop), "N of M" counter, distinct visual
   treatment of the two sides, ARIA dialog semantics + focus, graceful error.
6. **Version history**: confirmation before restore, "current" clearly tagged, restore
   errors inline without destroying the button, relative timestamps.
7. **Pairing code**: copy-to-clipboard, errors in a toast (not the code font), clear on
   regenerate, surface expiry.
8. **Mount form**: inline validation, ARIA tab roles, disable Unmount when not mounted,
   busy state on mount.

**P2 — later:**
9. Collapsible folders, file-type icons.
10. Surface the active mount backend per OS.
11. Remove the dead scaffold (`main.js`).

## 5. Constraints honored
- No new dependencies (vanilla HTML/CSS/JS only).
- The Tauri `invoke` command names and argument casing are **unchanged** — verified
  against `src-tauri/src/lib.rs`.
