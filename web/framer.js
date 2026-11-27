// The banner framer: the whole banner picture with a bright box over the part the header shows,
// which a person drags (or moves with the arrow keys) and enlarges with a size slider. Library
// settings frames a library's banner picture with it; Change the look the site's header image.
// The framing is `{x, y, zoom}` as hero.js describes it; this module only draws and moves it.

import { applyHeroFrame, heroFrame, heroWindow, HERO_FRAME_DEFAULT, HERO_ZOOM_MAX, HERO_ZOOM_MIN, moveHeroFrame, sameFrame } from './hero.js';

/**
 * Wires a framer made of `stage` (focusable, holding `picture` and the bright box `window`),
 * the `zoom` slider with its `zoomText` and the `reset` button. `headerBox()` answers the size
 * the picture is framed for (`{width, height}`), `live()` a header picture to frame alike as it
 * moves (or null), and `onChange(frame)` runs on each change a person makes. Answers
 * `{show(url, frame), redraw(), frame}`: `show` sets the picture and framing without calling
 * `onChange`, `frame` is the framing now.
 */
export function mountFramer({ stage, picture, window: box, zoom, zoomText, reset, headerBox, live = () => null, onChange }) {
  let framing = { ...HERO_FRAME_DEFAULT };

  const size = () => ({ width: picture.naturalWidth, height: picture.naturalHeight });

  /** Draws the bright box and the size from the framing, and frames the live picture alike. */
  const draw = () => {
    zoom.value = String(framing.zoom);
    zoomText.textContent = `${framing.zoom}%`;
    applyHeroFrame(live(), framing);
    if (!picture.naturalWidth || !picture.naturalHeight) return;
    // The stage takes the picture's own shape, at most 280 pixels high.
    stage.style.setProperty('--ratio', String(picture.naturalWidth / picture.naturalHeight));
    const shown = heroWindow(framing, headerBox(), size());
    box.style.left = `${shown.left * 100}%`;
    box.style.top = `${shown.top * 100}%`;
    box.style.width = `${shown.width * 100}%`;
    box.style.height = `${shown.height * 100}%`;
  };

  const set = (frame) => {
    const next = heroFrame(frame);
    if (sameFrame(next, framing)) return;
    framing = next;
    draw();
    onChange?.({ ...framing });
  };

  /** Moves the bright box by `dx` and `dy` (fractions of the picture). */
  const move = (dx, dy) => {
    if (!picture.naturalWidth || !picture.naturalHeight) return;
    set(moveHeroFrame(framing, dx, dy, headerBox(), size()));
  };

  let last = null;
  stage.addEventListener('pointerdown', (event) => {
    if (event.button !== 0) return;
    last = { x: event.clientX, y: event.clientY };
    stage.setPointerCapture(event.pointerId);
    stage.classList.add('dragging');
    event.preventDefault();
    stage.focus({ preventScroll: true });
  });
  stage.addEventListener('pointermove', (event) => {
    if (!last) return;
    const rect = picture.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    move((event.clientX - last.x) / rect.width, (event.clientY - last.y) / rect.height);
    last = { x: event.clientX, y: event.clientY };
  });
  const stop = () => {
    last = null;
    stage.classList.remove('dragging');
  };
  stage.addEventListener('pointerup', stop);
  stage.addEventListener('pointercancel', stop);
  stage.addEventListener('keydown', (event) => {
    const step = event.shiftKey ? 0.1 : 0.02;
    const moves = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
    if (moves[event.key]) {
      event.preventDefault();
      move(...moves[event.key]);
    } else if (event.key === '+' || event.key === '=' || event.key === '-') {
      event.preventDefault();
      const next = framing.zoom + (event.key === '-' ? -10 : 10);
      set({ ...framing, zoom: Math.min(HERO_ZOOM_MAX, Math.max(HERO_ZOOM_MIN, next)) });
    }
  });
  zoom.addEventListener('input', () => set({ ...framing, zoom: Number(zoom.value) }));
  reset.addEventListener('click', () => set(HERO_FRAME_DEFAULT));
  globalThis.addEventListener?.('resize', () => {
    if (stage.isConnected && stage.offsetParent !== null) draw();
  });

  return {
    show(url, frame) {
      framing = heroFrame(frame);
      if (url && picture.getAttribute('src') !== url) {
        picture.src = url;
        picture.addEventListener('load', draw, { once: true });
      }
      draw();
    },
    redraw: draw,
    get frame() {
      return { ...framing };
    },
  };
}
