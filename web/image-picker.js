// The image picker: a dialog listing the image files of the asset libraries the person can view
// (and, on Change the look, the libraries of that site, and the brand centre's images), with
// thumbnails, a name filter and keyboard use. The listing and the filter are pure and tested;
// the server judges every image again when it is chosen.

import { api, h, seg, showThumbnail } from './common.js';

/** The file names brand images may have: the types the server takes (judged again by content). */
const IMAGE_NAME = /\.(png|jpe?g|gif|webp|svg)$/i;
/** Types the server makes thumbnails of; an SVG is shown from its preview instead. */
const THUMBNAILED = /\.(png|jpe?g|gif|webp)$/i;
/** At most this many folders are opened while listing, so a huge library cannot stall the dialog. */
const MAX_FOLDERS = 200;
/** At most this many images are listed. */
const MAX_IMAGES = 2000;

/** Whether `name` is the name of an image file a brand may use. */
export const isImageName = (name) => typeof name === 'string' && IMAGE_NAME.test(name);

/**
 * The image files among `items` (a page of `GET /api/nodes/{id}/children`) of `library`
 * (`{name}`), by name: documents with an image type, not held, quarantined or blocked. Each
 * `{doc, name, library, thumb}`, `thumb` being the thumbnail route (or an SVG's preview).
 */
export function imageEntries(items, library) {
  return (Array.isArray(items) ? items : [])
    .filter((item) => item?.kind === 'document' && isImageName(item.name))
    .filter((item) => !item.scan && item.policy !== 'blocked')
    .map((item) => ({
      doc: item.id,
      name: item.name,
      library: String(library?.name ?? ''),
      thumb: `/api/docs/${seg(item.id)}/${THUMBNAILED.test(item.name) ? 'thumbnail' : 'preview'}`,
    }))
    .sort((a, b) => a.name.localeCompare(b.name) || a.library.localeCompare(b.library));
}

/** The entries whose name or library holds `text` (trimmed, ignoring case); all for none. */
export function filterImages(entries, text) {
  const wanted = String(text ?? '').trim().toLocaleLowerCase();
  if (!wanted) return entries;
  return entries.filter(
    (entry) => entry.name.toLocaleLowerCase().includes(wanted) || String(entry.library ?? '').toLocaleLowerCase().includes(wanted),
  );
}

/** Every child of `nodeId` the person can see, page by page; empty when it cannot be read. */
async function children(nodeId) {
  const items = [];
  let after = null;
  for (;;) {
    const query = `limit=500${after ? `&after=${encodeURIComponent(after)}` : ''}`;
    const result = await api('GET', `/nodes/${seg(nodeId)}/children?${query}`);
    if (!result.ok) return items;
    items.push(...result.data.items);
    after = result.data.next;
    if (!after) return items;
  }
}

/** The libraries to list: the asset libraries the person can view, then `siteId`'s libraries. */
async function librariesFor(siteId) {
  const libraries = [];
  const assets = await api('GET', '/assets');
  if (assets.ok && Array.isArray(assets.data)) libraries.push(...assets.data.map((l) => ({ id: l.id, name: l.name })));
  if (siteId) {
    for (const item of await children(siteId)) {
      if (item.kind === 'library' && !libraries.some((l) => l.id === item.id)) libraries.push({ id: item.id, name: item.name });
    }
  }
  return libraries;
}

/** The image entries of `libraries`, their folders opened breadth first within the limits. */
async function gather(libraries) {
  const entries = [];
  let opened = 0;
  for (const library of libraries) {
    const queue = [library.id];
    while (queue.length && opened < MAX_FOLDERS && entries.length < MAX_IMAGES) {
      const items = await children(queue.shift());
      opened += 1;
      entries.push(...imageEntries(items, library));
      for (const item of items) if (item.kind === 'folder') queue.push(item.id);
    }
  }
  return entries.slice(0, MAX_IMAGES).sort((a, b) => a.name.localeCompare(b.name) || a.library.localeCompare(b.library));
}

/** Arrow keys, Home and End move the focus among the dialog's choices. */
function arrowKeys(list) {
  list.addEventListener('keydown', (event) => {
    const buttons = Array.from(list.querySelectorAll('button.pick'));
    const at = buttons.indexOf(document.activeElement);
    if (at < 0) return;
    const moves = { ArrowRight: at + 1, ArrowDown: at + 1, ArrowLeft: at - 1, ArrowUp: at - 1, Home: 0, End: buttons.length - 1 };
    if (!(event.key in moves)) return;
    event.preventDefault();
    buttons[Math.max(0, Math.min(buttons.length - 1, moves[event.key]))]?.focus();
  });
}

/**
 * Opens the picker. `siteId` adds that site's libraries; `brandImages` (the brand centre's
 * images, `{id, name, url}`) are offered first. Resolves `{doc, name}` for a library file,
 * `{brand_image, sha256, name}` for a brand image, or null for Cancel or Escape.
 */
export function pickImage({ siteId = null, brandImages = [], title = 'Choose an image' } = {}) {
  return new Promise((resolve) => {
    const filter = h('input', { type: 'text', id: 'picker-filter', autocomplete: 'off', 'aria-describedby': 'picker-status' });
    const status = h('p', { id: 'picker-status', class: 'meta', role: 'status', 'aria-live': 'polite' }, 'Looking for images…');
    const list = h('ul', { class: 'list grid picker', 'aria-label': 'Images' });
    const cancel = h('button', { type: 'button', class: 'plain' }, 'Cancel');
    const dialog = h(
      'dialog',
      { 'aria-label': title, class: 'picker-dialog' },
      h('h2', {}, title),
      h('div', { class: 'field' }, h('label', { for: 'picker-filter' }, 'Filter by name'), filter),
      status,
      list,
      h('div', { class: 'row end' }, cancel),
    );
    let answered = false;
    const answer = (choice) => {
      if (answered) return;
      answered = true;
      if (dialog.open) dialog.close();
      dialog.remove();
      resolve(choice);
    };
    cancel.addEventListener('click', () => answer(null));
    dialog.addEventListener('close', () => answer(null));
    arrowKeys(list);

    const brand = (brandImages ?? [])
      .filter((image) => typeof image?.url === 'string')
      .map((image) => ({
        brand_image: image.id,
        sha256: image.url.slice(-64),
        name: image.name,
        library: 'Brand centre',
        thumb: image.url,
      }));
    let entries = brand;
    const tile = (entry) => {
      const picture = h('span', { class: 'thumb', 'aria-hidden': 'true' });
      const button = h(
        'button',
        { type: 'button', class: 'pick plain' },
        picture,
        h('span', { class: 'tile-name' }, entry.name),
        h('small', {}, entry.library),
      );
      button.addEventListener('click', () =>
        answer(entry.brand_image ? { brand_image: entry.brand_image, sha256: entry.sha256, name: entry.name } : { doc: entry.doc, name: entry.name }),
      );
      showThumbnail(picture, entry.thumb);
      return h('li', {}, button);
    };
    const show = (done) => {
      const shown = filterImages(entries, filter.value);
      list.replaceChildren(...shown.map(tile));
      if (!done) status.textContent = 'Looking for images…';
      else if (!entries.length) status.textContent = 'There are no images in the libraries you can see.';
      else status.textContent = shown.length === 1 ? '1 image' : `${shown.length} images`;
    };
    let loaded = false;
    filter.addEventListener('input', () => show(loaded));
    document.body.append(dialog);
    dialog.showModal();
    filter.focus();
    show(false);
    librariesFor(siteId)
      .then(gather)
      .then((found) => {
        entries = brand.concat(found);
        loaded = true;
        if (!answered) show(true);
      });
  });
}
