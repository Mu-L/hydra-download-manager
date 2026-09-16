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

// The decision point — and the last moment before the browser commits to a
// file of its own.
//
// Chromium asks extensions for a filename BEFORE it reserves the path and,
// with "Ask where to save each file" on, before it puts up the "Save as"
// dialog. Answering straight away and deciding afterwards therefore raced
// the browser's own save UI to the screen, and lost: users saw the picker
// open behind Hydra's New Download window on every capture. Returning true
// is what holds target determination open until the round-trip to the app is
// done; by then the download Hydra took is already cancelled and there is
// nothing left for the dialog to ask about.
//
// `suggest` is still called exactly once on every path, as the API requires:
// on a declined download it releases the browser to save it normally, and on
// one Hydra took it lands on a cancelled item and does nothing. Skipping it
// to avoid that no-op would leave a download stuck in target determination
// for good on any path where the cancel did not take.
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  decideCapture(item, true)
    .catch((e) => console.debug(`hydra: capture decision failed — ${e}`))
    .finally(() => {
      try {
        suggest();
      } catch {
        // The item is gone: Hydra took it and the callback outlived it.
      }
    });
  return true;
});
