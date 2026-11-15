// The library's file table: what kind of file each entry is (and its icon), when it was last
// changed in words people use ("5 minutes ago"), and the order the table is sorted in. The
// helpers here are pure apart from `fileIcon`, which builds an inline SVG.

import { regionalSettings } from './common.js';

const EXTENSIONS = {
  image: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'tif', 'tiff', 'ico', 'heic', 'heif', 'avif'],
  audio: ['mp3', 'wav', 'ogg', 'oga', 'flac', 'm4a', 'aac', 'opus', 'wma', 'aif', 'aiff', 'mid', 'midi'],
  video: ['mp4', 'webm', 'mov', 'mkv', 'avi', 'm4v', 'wmv', 'mpg', 'mpeg', 'ogv', '3gp'],
  archive: ['zip', '7z', 'rar', 'tar', 'gz', 'tgz', 'bz2', 'xz', 'zst', 'cab', 'iso'],
  word: ['docx', 'doc', 'odt', 'rtf'],
  sheet: ['xlsx', 'xls', 'ods', 'csv', 'tsv'],
  pdf: ['pdf'],
  model: ['stl', 'step', 'stp', '3mf', 'obj', 'iges', 'igs'],
  text: ['txt', 'md', 'markdown', 'log', 'json', 'xml', 'yaml', 'yml', 'toml', 'ini'],
};

const BY_EDITOR = { text: 'text', wordcraft: 'word', gridcraft: 'sheet', pdfcraft: 'pdf', solidcraft: 'model' };

/** Every icon kind, in no particular order. */
export const FILE_KINDS = Object.freeze(['folder', 'text', 'word', 'sheet', 'pdf', 'model', 'image', 'audio', 'video', 'archive', 'file']);

const TYPE_NAMES = {
  folder: 'Folder',
  library: 'Library',
  text: 'Text',
  word: 'Document',
  sheet: 'Spreadsheet',
  pdf: 'PDF',
  model: '3D model',
  image: 'Image',
  audio: 'Audio',
  video: 'Video',
  archive: 'Archive',
  file: 'File',
};

/** The lower-case extension of `name` without its dot, or '' when it has none. */
function extensionOf(name) {
  const dot = typeof name === 'string' ? name.lastIndexOf('.') : -1;
  return dot > 0 && dot < name.length - 1 ? name.slice(dot + 1).toLowerCase() : '';
}

/**
 * The icon kind of an entry: `folder` for a folder or library (`nodeKind`), otherwise by the
 * editor that opens it (`text`, `word`, `sheet`, `pdf`, `model`) and, for any other file, by the
 * ending of its name (`image`, `audio`, `video`, `archive`, …), `file` when nothing fits.
 */
export function fileKind(name, editor, nodeKind = 'document') {
  if (nodeKind === 'folder' || nodeKind === 'library') return 'folder';
  if (Object.hasOwn(BY_EDITOR, editor ?? '')) return BY_EDITOR[editor];
  const extension = extensionOf(name);
  for (const [kind, endings] of Object.entries(EXTENSIONS)) {
    if (endings.includes(extension)) return kind;
  }
  return 'file';
}

/**
 * The Type column's words for an entry: `Folder`, `Spreadsheet`, `PDF`; an image, sound, video,
 * archive or other file names its ending too (`PNG image`, `ZIP archive`, `BIN file`).
 */
export function typeLabel(item) {
  if (item.kind === 'library') return TYPE_NAMES.library;
  const kind = fileKind(item.name, item.editor, item.kind);
  const extension = extensionOf(item.name);
  if (['image', 'audio', 'video', 'archive', 'file'].includes(kind) && extension && extension.length <= 5) {
    const word = kind === 'audio' ? 'audio' : TYPE_NAMES[kind].toLowerCase();
    return `${extension.toUpperCase()} ${word}`;
  }
  return TYPE_NAMES[kind];
}

const DAY = 24 * 60 * 60 * 1000;

/** The calendar day of `ms` in `timeZone` as a number of days, to tell today from yesterday. */
function dayNumber(ms, timeZone) {
  const parts = new Intl.DateTimeFormat('en-GB', { year: 'numeric', month: 'numeric', day: 'numeric', timeZone }).formatToParts(new Date(ms));
  const part = (type) => Number(parts.find((p) => p.type === type).value);
  return Math.round(Date.UTC(part('year'), part('month') - 1, part('day')) / DAY);
}

/**
 * `date` (a Date, milliseconds or an RFC 3339 string) as words relative to `now`: `just now`,
 * `1 minute ago`, `5 minutes ago`, `3 hours ago` (the same day, or within six hours),
 * `yesterday`, then the day and month in `language` (`12 Nov`), with the year when it is not
 * this year's. A time a little ahead of `now` (clocks differ) is `just now`. '' for no date.
 */
export function relativeTime(date, now = Date.now(), { language, timeZone } = regionalSettings()) {
  if (date == null || date === '') return '';
  const at = new Date(date).getTime();
  const current = new Date(now).getTime();
  if (Number.isNaN(at)) return String(date);
  const ago = current - at;
  if (ago < 60_000 && ago > -5 * 60_000) return 'just now';
  if (ago >= 60_000) {
    const minutes = Math.floor(ago / 60_000);
    if (minutes < 60) return minutes === 1 ? '1 minute ago' : `${minutes} minutes ago`;
    const days = dayNumber(current, timeZone) - dayNumber(at, timeZone);
    const hours = Math.floor(minutes / 60);
    if (days === 0 || hours < 6) return hours === 1 ? '1 hour ago' : `${hours} hours ago`;
    if (days === 1) return 'yesterday';
  }
  const options = { day: 'numeric', month: 'short', timeZone };
  const year = (ms) => new Intl.DateTimeFormat('en-GB', { year: 'numeric', timeZone }).format(new Date(ms));
  if (year(at) !== year(current)) options.year = 'numeric';
  return new Intl.DateTimeFormat(language ?? 'en-GB', options).format(new Date(at));
}

/** The columns the table sorts by, and the direction each starts in when first chosen. */
export const SORT_KEYS = Object.freeze({ name: 'ascending', modified: 'descending', type: 'ascending' });

export const DEFAULT_SORT = Object.freeze({ key: 'name', direction: 'ascending' });

/** A remembered sort (`{ key, direction }`, or its JSON) if it is a valid one, else the default. */
export function parseSort(value) {
  let sort = value;
  if (typeof value === 'string') {
    try {
      sort = JSON.parse(value);
    } catch {
      return DEFAULT_SORT;
    }
  }
  if (sort && Object.hasOwn(SORT_KEYS, sort.key) && ['ascending', 'descending'].includes(sort.direction)) {
    return { key: sort.key, direction: sort.direction };
  }
  return DEFAULT_SORT;
}

/** The sort after a column header is pressed: the other direction on the same column. */
export function nextSort(current, key) {
  if (current.key === key) return { key, direction: current.direction === 'ascending' ? 'descending' : 'ascending' };
  return { key, direction: SORT_KEYS[key] };
}

/** The sentence announced after sorting: `Sorted by name, A to Z.` */
export function sortDescription({ key, direction }) {
  const up = direction === 'ascending';
  if (key === 'modified') return `Sorted by modified, ${up ? 'oldest' : 'newest'} first.`;
  if (key === 'type') return `Sorted by type, ${up ? 'A to Z' : 'Z to A'}.`;
  return `Sorted by name, ${up ? 'A to Z' : 'Z to A'}.`;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
const isFolder = (entry) => entry.kind === 'folder' || entry.kind === 'library';

/**
 * `entries` (children as the server lists them) sorted by `key` (`name`, `modified` or `type`)
 * in `direction` (`ascending` or `descending`), as a new array. Folders always come first; ties
 * go by name, A to Z. An entry with no modified time (a folder, a file never saved) goes after
 * those that have one, whichever the direction.
 */
export function sortEntries(entries, key = 'name', direction = 'ascending') {
  const sign = direction === 'descending' ? -1 : 1;
  const byName = (a, b) => collator.compare(a.name, b.name);
  const time = (entry) => {
    const ms = entry.revised_at ? new Date(entry.revised_at).getTime() : NaN;
    return Number.isNaN(ms) ? null : ms;
  };
  const compare = (a, b) => {
    if (isFolder(a) !== isFolder(b)) return isFolder(a) ? -1 : 1;
    if (key === 'modified') {
      const [x, y] = [time(a), time(b)];
      if (x !== y) {
        if (x === null) return 1;
        if (y === null) return -1;
        return sign * (x - y);
      }
      return byName(a, b);
    }
    if (key === 'type') return sign * collator.compare(typeLabel(a), typeLabel(b)) || byName(a, b);
    return sign * byName(a, b) || collator.compare(a.id ?? '', b.id ?? '');
  };
  return [...entries].sort(compare);
}

// ---- icons ----

const SVG = 'http://www.w3.org/2000/svg';
const PAGE = 'M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2z';

// Each kind: shapes in the kind's colour ("fill") and marks drawn on top in the ink ("ink" fills
// and "line" strokes). Every icon is decorative: the Type column says the same in words.
const SHAPES = {
  folder: [['fill', 'path', { d: 'M2 6.5A2.5 2.5 0 0 1 4.5 4h4.2l2.3 2.3h8.5A2.5 2.5 0 0 1 22 8.8v9.7a2.5 2.5 0 0 1-2.5 2.5h-15A2.5 2.5 0 0 1 2 18.5z' }]],
  text: [['line', 'path', { d: 'M8 12h8M8 15h8M8 18h5' }]],
  word: [['line', 'path', { d: 'M7.5 11.5l1.6 6.5 1.9-5 1.9 5 1.6-6.5' }]],
  sheet: [['line', 'path', { d: 'M7.5 11.5h9v7h-9zM7.5 15h9M12 11.5v7' }]],
  pdf: [['ink', 'rect', { x: 6.5, y: 13, width: 11, height: 5, rx: 1 }]],
  model: [['line', 'path', { d: 'M12 10.5l4 2v4.5l-4 2-4-2v-4.5zM8 12.5l4 2 4-2M12 14.5v4.5' }]],
  image: [
    ['ink', 'circle', { cx: 9.5, cy: 12, r: 1.6 }],
    ['ink', 'path', { d: 'M6.5 19l3.5-3.5 2.5 2.5 2-2 3 3z' }],
  ],
  audio: [
    ['ink', 'circle', { cx: 10, cy: 17.5, r: 1.8 }],
    ['line', 'path', { d: 'M11.6 17.5V11l3.4-1v2.2' }],
  ],
  video: [['ink', 'path', { d: 'M10 11.2v7l6-3.5z' }]],
  archive: [
    ['ink', 'path', { d: 'M11 3h2v1.5h-2zM12 4.5h2V6h-2zM11 6h2v1.5h-2zM12 7.5h2V9h-2z' }],
    ['line', 'rect', { x: 10.5, y: 11, width: 3, height: 4, rx: 0.8 }],
  ],
  file: [],
};

/**
 * The type icon of `kind` (see {@link fileKind}) as a 24 by 24 inline SVG, hidden from screen
 * readers. Its colours come from the stylesheet (`.file-icon.k-<kind>`), in light and dark.
 */
export function fileIcon(kind, doc = globalThis.document) {
  const which = Object.hasOwn(SHAPES, kind) ? kind : 'file';
  const make = (tag, attributes) => {
    const element = doc.createElementNS(SVG, tag);
    for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
    return element;
  };
  const svg = make('svg', {
    viewBox: '0 0 24 24',
    width: 24,
    height: 24,
    class: `file-icon k-${which}`,
    'aria-hidden': 'true',
    focusable: 'false',
  });
  if (which !== 'folder') {
    svg.append(make('path', { d: PAGE, class: 'fill' }), make('path', { d: 'M14 2v4a2 2 0 0 0 2 2h4', class: 'fold' }));
  }
  for (const [role, tag, attributes] of SHAPES[which]) svg.append(make(tag, { ...attributes, class: role }));
  return svg;
}
