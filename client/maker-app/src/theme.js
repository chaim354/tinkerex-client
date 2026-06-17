// Theme switcher — applies + persists the selected visual theme.
//
// The real normalization logic (unknown name -> "default") is also owned + unit-
// tested in the Rust layer (Prefs::normalized_theme / THEMES in src-tauri/src/lib.rs);
// this is the thin frontend mirror. localStorage is the instant, no-FOUC fast path;
// the durable mirror is Prefs.theme (set when the user picks a theme, in settings.js).
//
// NOTE: a tiny inline <script> in index.html's <head> applies the saved theme BEFORE
// first paint to avoid a flash. This module re-applies on load (idempotent) and is the
// canonical home for the constants + applyTheme so settings.js can import them.

export const THEMES = ["default", "hacker", "woodworker", "craftmom"];
export const THEME_KEY = "maker.theme";

/** A theme's display label + the three preview-swatch colors (token names). */
export const THEME_META = {
  default: { label: "Default", note: "Clean, follows your system light/dark" },
  hacker: { label: "Hacker", note: "Terminal green-on-black, monospace" },
  woodworker: { label: "Woodworker", note: "Warm walnut & oak tones, serif" },
  craftmom: { label: "Craft mom", note: "Soft pastels, extra-rounded" },
};

/** Normalize an arbitrary name to a known theme. Unknown/empty -> "default". */
export function normalizeTheme(name) {
  return THEMES.includes(name) ? name : "default";
}

/**
 * Apply a theme: set <html data-theme> and persist to localStorage.
 * Returns the (normalized) theme actually applied.
 */
export function applyTheme(name) {
  const t = normalizeTheme(name);
  document.documentElement.setAttribute("data-theme", t);
  try {
    localStorage.setItem(THEME_KEY, t);
  } catch {
    /* localStorage may be unavailable (private mode / purge) — apply still works */
  }
  // let other surfaces (e.g. a live sample in Settings) react
  window.dispatchEvent(new CustomEvent("theme:change", { detail: t }));
  return t;
}

/** The currently-saved theme (normalized), or "default" if none/unreadable. */
export function currentTheme() {
  let saved = null;
  try {
    saved = localStorage.getItem(THEME_KEY);
  } catch {
    /* ignore */
  }
  return normalizeTheme(saved);
}

// Boot: re-apply the saved theme (idempotent with the inline <head> shim).
applyTheme(currentTheme());
