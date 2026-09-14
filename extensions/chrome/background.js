// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Hydra browser integration, Chromium service worker: Chrome, Edge, Brave,
// Vivaldi, Opera, Arc. Everything browser-neutral is core.js; this file is
// only the Chromium way of capturing a download.
//
// Chromium can PARK a download: `downloads.pause()` on creation is
// reversible, so the decision can wait for `onDeterminingFilename`, where
// the real name (Content-Disposition applied) is known, and a download the
// gates turn away is resumed untouched. Small files cannot slip through by
// finishing early, and signed one-shot URLs keep working because the bytes
// are never re-requested. Gecko has none of this — see firefox/background.js.
importScripts("core.js");

// The cheap half of `offerToHydra`'s gates: what can be decided without asking
// the app anything, which is all parking is allowed to cost.
function captureEligible(item, state) {
  const url = captureUrl(item);
  return (
    state.enabled &&
    state.guiCapture &&
    /^(https?|ftp):/i.test(url) &&
    item.byExtensionId !== chrome.runtime.id
  );
}

// Freeze the download the moment it exists.
async function parkDownload(item) {
  const state = await getState();
  if (!captureEligible(item, state)) return;
  try {
    await chrome.downloads.pause(item.id);
  } catch {
    // Finished already or not pausable; the decision step handles it.
  }
}

chrome.downloads.onCreated.addListener((item) => parkDownload(item));
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  suggest();
  decideCapture(item, true);
});
