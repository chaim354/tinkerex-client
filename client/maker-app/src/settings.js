// Settings surface: file-layer preferences (bandwidth caps + progressive hydration).
// These are persisted per account and applied at the next sign-in/mount (they're
// DaemonConfig start-time options). Selective sync is plan-only (see index.html note).
//
// Tauri commands:
//   get_prefs {serverUrl,email} -> { down_mbps, up_mbps, progressive_hydration }
//   set_prefs {serverUrl,email,prefs}

const UI = window.MakerUI;
const { invoke, $ } = UI;
const a = () => UI.args();

async function loadPrefs() {
  let prefs;
  // set_prefs/get_prefs take NO device token — they key a file on (serverUrl, email) alone — so
  // signed out these were the one pair that reached a real account's prefs.json on any box, not
  // just a developer's. Don't ask, and refuse the write below.
  if (!UI.signedIn()) {
    $("bwDown").value = 0;
    $("bwUp").value = 0;
    $("progHydrate").checked = false;
    return;
  }
  try {
    prefs = await invoke("get_prefs", a());
  } catch {
    prefs = { down_mbps: 0, up_mbps: 0, progressive_hydration: false };
  }
  $("bwDown").value = prefs.down_mbps ?? 0;
  $("bwUp").value = prefs.up_mbps ?? 0;
  $("progHydrate").checked = !!prefs.progressive_hydration;
}

$("prefsSave").onclick = async () => {
  if (!UI.signedIn()) return UI.toast($("settingsToast"), UI.SIGNED_OUT_HINT, "err");
  const prefs = {
    down_mbps: Math.max(0, Number($("bwDown").value) || 0),
    up_mbps: Math.max(0, Number($("bwUp").value) || 0),
    progressive_hydration: $("progHydrate").checked,
  };
  await UI.withBusy($("prefsSave"), "Saving…", async () => {
    try {
      const msg = await invoke("set_prefs", { ...a(), prefs });
      UI.toast($("settingsToast"), msg, "ok");
    } catch (e) {
      UI.toast($("settingsToast"), String(e), "err");
    }
  });
};

window.addEventListener("view:show", (e) => {
  if (e.detail === "settings") loadPrefs();
});
