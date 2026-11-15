// Light or dark, as this viewer chose it: System (follow the device), Light or Dark. Loaded as a
// plain (not module) script in the <head> of every page, so it runs before the page is drawn and
// there is no flash of the wrong colours. The choice is kept per browser in localStorage; style.css
// colours everything with light-dark(), so setting color-scheme on <html> is all it takes.
// Editors inside file.html are separate documents and keep following the device.
//
// The page's scripts change it through `globalThis.sharecraftAppearance.set(choice)`.
(function () {
  'use strict';
  var KEY = 'sharecraft-appearance';
  var CHOICES = ['system', 'light', 'dark'];

  /** `stored` if it is a known choice, else 'system'. */
  function normalise(stored) {
    return CHOICES.indexOf(stored) >= 0 ? stored : 'system';
  }

  /**
   * The choice (`system`, `light` or `dark`; anything else counts as `system`) and the scheme it
   * shows (`light` or `dark`), given whether the device prefers dark.
   */
  function resolveAppearance(stored, systemPrefersDark) {
    var choice = normalise(stored);
    var scheme = choice === 'system' ? (systemPrefersDark ? 'dark' : 'light') : choice;
    return { choice: choice, scheme: scheme };
  }

  function read() {
    try {
      return normalise(globalThis.localStorage.getItem(KEY));
    } catch (e) {
      return 'system';
    }
  }

  var media = null;
  try {
    media = globalThis.matchMedia('(prefers-color-scheme: dark)');
  } catch (e) {
    media = null;
  }

  var current = read();

  function apply(choice) {
    var result = resolveAppearance(choice, Boolean(media && media.matches));
    var root = globalThis.document && globalThis.document.documentElement;
    if (root) {
      // Through CSSOM, which the page's Content-Security-Policy allows (a style attribute would not be).
      if (result.choice === 'system') root.style.removeProperty('color-scheme');
      else root.style.setProperty('color-scheme', result.scheme);
      root.setAttribute('data-appearance', result.scheme);
    }
    return result;
  }

  function set(choice) {
    current = normalise(choice);
    try {
      if (current === 'system') globalThis.localStorage.removeItem(KEY);
      else globalThis.localStorage.setItem(KEY, current);
    } catch (e) {
      // not kept (private window, blocked storage): it still applies to this page
    }
    return apply(current);
  }

  apply(current);
  if (media && media.addEventListener) {
    media.addEventListener('change', function () {
      if (current === 'system') apply(current);
    });
  }
  // Another tab of this browser changed it.
  if (globalThis.addEventListener) {
    globalThis.addEventListener('storage', function (event) {
      if (event.key !== KEY && event.key !== null) return;
      current = read();
      apply(current);
    });
  }

  globalThis.sharecraftAppearance = {
    KEY: KEY,
    CHOICES: CHOICES.slice(),
    resolveAppearance: resolveAppearance,
    get: function () {
      return current;
    },
    set: set,
  };
})();
