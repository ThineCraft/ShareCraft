// The recycle bin's list, as pure functions: entries grouped under date headings by when they
// were deleted, how urgent the days-left clock is, the header's counts, the search and filters,
// the type filter's choices and the words said when a restored item could not go back to its
// own folder. The page (bin.js) keeps the state and draws it.

import { formatSize } from './common.js';
import { fileKind } from './files.js';

/** How many days a deleted item stays in the recycle bin before it is erased. */
export const BIN_DAYS = 93;

/** The last days before erasing, when the clock turns to the warning colour. */
export const WARN_DAYS = 7;

const DAY = 24 * 60 * 60 * 1000;

const GROUPS = [
  { id: 'today', heading: 'Today' },
  { id: 'yesterday', heading: 'Yesterday' },
  { id: 'week', heading: 'Earlier this week' },
  { id: 'month', heading: 'Earlier this month' },
  { id: 'older', heading: 'Older' },
];

/** The calendar day of `ms` in `timeZone`: `{ day }` (days since 1970) and `{ year, month }`. */
function calendarDay(ms, timeZone) {
  let parts;
  try {
    parts = new Intl.DateTimeFormat('en-GB', { year: 'numeric', month: 'numeric', day: 'numeric', timeZone }).formatToParts(new Date(ms));
  } catch {
    parts = new Intl.DateTimeFormat('en-GB', { year: 'numeric', month: 'numeric', day: 'numeric' }).formatToParts(new Date(ms));
  }
  const part = (type) => Number(parts.find((p) => p.type === type).value);
  const year = part('year');
  const month = part('month');
  return { year, month, day: Math.round(Date.UTC(year, month - 1, part('day')) / DAY) };
}

/** The day number of the Monday that starts the week of day number `day` (1 Jan 1970 was a Thursday). */
const mondayOf = (day) => day - (((day + 3) % 7) + 7) % 7;

/**
 * `items` (bin entries with `deleted_at`, newest first) under the headings people use, by the
 * calendar day in `timeZone` they were deleted compared with `now`: Today, Yesterday, Earlier
 * this week (weeks start on Monday), Earlier this month, Older. Answers only the groups that
 * have items, in that order: `[{ id, heading, items }]`, each keeping the order given. A time a
 * little ahead of `now` (clocks differ) is today; an entry without a time is older.
 */
export function groupByDeletedDate(items, now = Date.now(), timeZone = undefined) {
  const today = calendarDay(new Date(now).getTime(), timeZone);
  const buckets = new Map(GROUPS.map((group) => [group.id, []]));
  for (const item of items ?? []) {
    const at = item?.deleted_at ? new Date(item.deleted_at).getTime() : Number.NaN;
    let id = 'older';
    if (!Number.isNaN(at)) {
      const then = calendarDay(at, timeZone);
      const ago = today.day - then.day;
      if (ago <= 0) id = 'today';
      else if (ago === 1) id = 'yesterday';
      else if (mondayOf(then.day) === mondayOf(today.day)) id = 'week';
      else if (then.year === today.year && then.month === today.month) id = 'month';
    }
    buckets.get(id).push(item);
  }
  return GROUPS.filter((group) => buckets.get(group.id).length > 0).map((group) => ({ ...group, items: buckets.get(group.id) }));
}

/**
 * How urgent the clock of an entry with `days` left is: `ok`, `warn` in the last
 * {@link WARN_DAYS} days, `due` when it is erased within a day.
 */
export function daysLeftLevel(days) {
  if (typeof days !== 'number' || Number.isNaN(days)) return 'ok';
  if (days <= 0) return 'due';
  if (days <= WARN_DAYS) return 'warn';
  return 'ok';
}

const itemsText = (n) => `${n} ${n === 1 ? 'item' : 'items'}`;

/** When the next entry is erased, in words: "in 3 days", "tomorrow", "within a day". */
function nextRemovalText(days) {
  if (days <= 0) return 'within a day';
  if (days === 1) return 'in 1 day';
  return `in ${days} days`;
}

/**
 * The header's counts for the entries in the bin: "14 items · 2.3 MB · next removal in 3
 * days". With `more` (not every page is loaded) the count says "500+ items". '' for none.
 */
export function binSummary(items, { more = false, language } = {}) {
  const list = items ?? [];
  if (list.length === 0) return '';
  const size = list.reduce((sum, item) => sum + (Number(item.size) || 0), 0);
  const count = more ? `${list.length}+ items` : itemsText(list.length);
  const days = list.map((item) => item.days_left).filter((d) => typeof d === 'number');
  const parts = [count, formatSize(size, language)];
  if (days.length > 0) parts.push(`next removal ${nextRemovalText(Math.min(...days))}`);
  return parts.join(' · ');
}

/** Where an entry was, as a quiet path: "Home › Documents › Plans". */
export function binPathText(entry) {
  return (entry?.path ?? []).join(' › ');
}

/** The type filter's kind of an entry: `library`, `folder`, or a file kind (see `fileKind`). */
export function binKind(entry) {
  if (entry?.kind === 'library') return 'library';
  return fileKind(entry?.name, entry?.editor, entry?.kind === 'folder' ? 'folder' : 'document');
}

const KIND_NAMES = {
  folder: 'Folders',
  library: 'Libraries',
  word: 'Documents',
  sheet: 'Spreadsheets',
  pdf: 'PDFs',
  text: 'Text files',
  image: 'Pictures',
  audio: 'Sound',
  video: 'Video',
  archive: 'Archives',
  model: '3D models',
  file: 'Other files',
};

/** The type filter's choices for `items`: only the kinds there are, `[{ value, label }]`, in a fixed order. */
export function kindChoices(items) {
  const present = new Set((items ?? []).map(binKind));
  return Object.keys(KIND_NAMES)
    .filter((kind) => present.has(kind))
    .map((kind) => ({ value: kind, label: KIND_NAMES[kind] }));
}

const fold = (text) => String(text ?? '').normalize('NFKD').replace(/\p{M}/gu, '').toLowerCase();

/**
 * The entries a search and the filters keep: `text` found in the name or the path (any case,
 * accents ignored, every word), `by` `me` for those deleted by `me` (an id), `kind` a
 * {@link binKind} or '' for any.
 */
export function filterBin(items, { text = '', by = 'anyone', kind = '', me = null } = {}) {
  const words = fold(text).split(/\s+/).filter(Boolean);
  return (items ?? []).filter((item) => {
    if (by === 'me' && item.deleted_by?.id !== me) return false;
    if (kind && binKind(item) !== kind) return false;
    if (words.length === 0) return true;
    const haystack = fold(`${item.name} ${(item.path ?? []).join(' ')}`);
    return words.every((word) => haystack.includes(word));
  });
}

/** Whether the search or a filter is narrowing the list. */
export const filtering = ({ text = '', by = 'anyone', kind = '' } = {}) => text.trim() !== '' || by === 'me' || kind !== '';

/** "Showing 3 of 14" while filtering, '' otherwise. */
export function shownText(shown, total, filters) {
  return filtering(filters) ? `Showing ${shown} of ${total}` : '';
}

/**
 * What is said about one restored item beyond "restored": that its name had to change, and
 * where it went when that is not where it was (`placePath`, the names from the site down to
 * where it is now, against `entry.path`), because a folder it was in is gone. With `alone` (the
 * only item restored, already named) it says "It went to …". '' when it went back where it was
 * under its own name.
 */
export function restoredNote(entry, placePath, renamed = null, { alone = false } = {}) {
  const notes = [];
  if (renamed?.from && renamed?.to) notes.push(`“${renamed.from}” is now “${renamed.to}”, because its name was taken.`);
  const was = entry?.path ?? [];
  const now = placePath ?? null;
  if (now && (now.length !== was.length || now.some((name, at) => name !== was[at]))) {
    const gone = was.find((name, at) => now[at] !== name);
    const why = gone ? `the folder “${gone}” it was in is gone` : 'the folder it was in is gone';
    notes.push(`${alone ? 'It' : `“${entry.name}”`} went to ${now.join(' › ')}, because ${why}.`);
  }
  return notes.join(' ');
}

/** The toast after restoring: "Restored “Plan.docx”." or "Restored 3 items.", with the notes after it. */
export function restoredText(restored, notes = []) {
  const what = restored.length === 1 ? `“${restored[0].name}”` : itemsText(restored.length);
  return [`Restored ${what}.`, ...notes.filter(Boolean)].join(' ');
}

/** The confirmation before erasing `count` selected items from the second stage for good. */
export function eraseText(count) {
  return `Permanently erase ${itemsText(count)}? Their files are erased from the server and this cannot be undone.`;
}

/** The empty bin's words, for a stage. */
export function emptyText(stage) {
  return stage === 'second'
    ? 'Nothing in the second stage. Items deleted from the recycle bin wait here until they are erased.'
    : `Nothing in the recycle bin. Deleted files stay here for ${BIN_DAYS} days.`;
}
