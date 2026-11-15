// Undo for what the library does through routes that can be reversed: a delete is restored from
// the recycle bin (`POST /api/bin/{id}/restore`, or `POST /api/bulk {op: 'restore'}`), a move is
// moved back where each item came from (`PATCH /api/nodes/{id} {parent}`, or a bulk move per
// place), a rename is renamed back (`PATCH /api/nodes/{id} {name}`). Pure, apart from the `call`
// given to performUndo (the page's `api`).

import { messageFor } from './common.js';
import { isTextField } from './palette.js';

export const UNDO_BATCH = 500; // the most one bulk request takes

const itemsText = (n) => `${n} ${n === 1 ? 'item' : 'items'}`;

function chunks(list, size) {
  const out = [];
  for (let at = 0; at < list.length; at += size) out.push(list.slice(at, at + size));
  return out;
}

/**
 * The undo of an action that was done, or null when there is nothing to undo.
 * - `{ op: 'delete', done: [ids] }` with `before` the items as they were (`[{ id, name }]`);
 * - `{ op: 'move', done: [ids], to: { id, name } }` with `before` `[{ id, name, parent }]`;
 * - `{ op: 'restore', done: [ids] }` (from the recycle bin) with `before` `[{ id, name }]`;
 * - `{ op: 'rename', id, name }` (the new name) with `before` `{ id, name }` (the old one).
 * Answers `{ op, message, label, requests, names, back }`: `message` is the toast's words,
 * `requests` the calls that reverse it (`{ method, path, body, ids, bulk }`).
 */
export function buildUndo(action, before) {
  if (!action || !before) return null;
  if (action.op === 'rename') {
    if (!before.id || before.name === action.name) return null;
    return {
      op: 'rename',
      message: `Renamed to ${action.name}`,
      label: 'Undo',
      names: { [before.id]: action.name },
      back: before.name,
      requests: [{ method: 'PATCH', path: `/nodes/${encodeURIComponent(before.id)}`, body: { name: before.name }, ids: [before.id], bulk: false }],
    };
  }
  if (action.op !== 'delete' && action.op !== 'move' && action.op !== 'restore') return null;
  const done = new Set(action.done ?? []);
  const items = (Array.isArray(before) ? before : []).filter((item) => done.has(item.id));
  if (items.length === 0) return null;
  const names = Object.fromEntries(items.map((item) => [item.id, item.name]));
  const what = items.length === 1 ? items[0].name : itemsText(items.length);
  if (action.op === 'delete') {
    const requests =
      items.length === 1
        ? [{ method: 'POST', path: `/bin/${encodeURIComponent(items[0].id)}/restore`, ids: [items[0].id], bulk: false }]
        : chunks(items.map((item) => item.id), UNDO_BATCH).map((ids) => ({
            method: 'POST',
            path: '/bulk',
            body: { op: 'restore', items: ids },
            ids,
            bulk: true,
          }));
    return { op: 'delete', message: `Deleted ${what}`, label: 'Undo', names, requests };
  }
  if (action.op === 'restore') {
    // Undoing a restore deletes the items again, through the normal delete route.
    const requests =
      items.length === 1
        ? [{ method: 'DELETE', path: `/nodes/${encodeURIComponent(items[0].id)}`, ids: [items[0].id], bulk: false }]
        : chunks(items.map((item) => item.id), UNDO_BATCH).map((ids) => ({
            method: 'POST',
            path: '/bulk',
            body: { op: 'delete', items: ids },
            ids,
            bulk: true,
          }));
    return { op: 'restore', message: `Restored ${what}`, label: 'Undo', names, requests };
  }
  // A move goes back per place the items came from.
  const byParent = new Map();
  for (const item of items) {
    if (!item.parent) continue;
    if (!byParent.has(item.parent)) byParent.set(item.parent, []);
    byParent.get(item.parent).push(item.id);
  }
  if (byParent.size === 0) return null;
  const requests = [];
  for (const [parent, ids] of byParent) {
    if (ids.length === 1) {
      requests.push({ method: 'PATCH', path: `/nodes/${encodeURIComponent(ids[0])}`, body: { parent }, ids, bulk: false });
    } else {
      for (const part of chunks(ids, UNDO_BATCH)) {
        requests.push({ method: 'POST', path: '/bulk', body: { op: 'move', items: part, parent }, ids: part, bulk: true });
      }
    }
  }
  const where = action.to?.name ? ` to ${action.to.name}` : '';
  return { op: 'move', message: `Moved ${what}${where}`, label: 'Undo', names, requests };
}

/** Each item's result of one undo request: `{ id, ok, error, renamed }`. */
export function undoResults(request, result) {
  if (request.bulk) {
    if (!result.ok || !Array.isArray(result.data)) {
      return request.ids.map((id) => ({ id, ok: false, error: result.error ?? 'unknown' }));
    }
    return result.data.map((r) => ({ id: r.id, ok: Boolean(r.ok), error: r.error ?? null, renamed: r.renamed ?? null }));
  }
  return [
    {
      id: request.ids[0],
      ok: Boolean(result.ok),
      error: result.ok ? null : (result.error ?? 'unknown'),
      renamed: result.ok ? (result.data?.renamed ?? null) : null,
    },
  ];
}

const UNDONE = { delete: 'restored', move: 'moved back', rename: 'renamed back', restore: 'moved back to the recycle bin' };

/** What happened when `undo` was tried: `{ ok, message }` in plain words for a toast. */
export function undoOutcome(undo, results) {
  const good = results.filter((r) => r.ok);
  const bad = results.filter((r) => !r.ok);
  const nameOf = (id) => undo.names?.[id] ?? 'An item';
  if (bad.length === 0) {
    if (undo.op === 'rename') return { ok: true, message: `Renamed back to ${undo.back}.` };
    const what = good.length === 1 ? nameOf(good[0].id) : itemsText(good.length);
    let message =
      undo.op === 'delete' ? `Restored ${what}.` : undo.op === 'restore' ? `Moved ${what} back to the recycle bin.` : `Moved ${what} back.`;
    for (const r of good) {
      if (r.renamed?.from && r.renamed?.to) {
        message += ` “${r.renamed.from}” is now “${r.renamed.to}”, because its name was taken.`;
      }
    }
    return { ok: true, message };
  }
  if (results.length === 1) return { ok: false, message: `Could not undo. ${messageFor(bad[0].error)}` };
  const shown = bad.slice(0, 3).map((r) => `${nameOf(r.id)}: ${messageFor(r.error)}`);
  if (bad.length > 3) shown.push(`and ${bad.length - 3} more.`);
  const head =
    good.length === 0
      ? `Could not undo. None of the ${results.length} items was ${UNDONE[undo.op]}.`
      : `Only ${good.length} of ${results.length} items were ${UNDONE[undo.op]}.`;
  return { ok: false, message: [head, ...shown].join(' ') };
}

/** Sends the requests of `undo` one after another through `call(method, path, body)`. */
export async function performUndo(undo, call) {
  const results = [];
  for (const request of undo.requests) {
    const result = await call(request.method, request.path, request.body);
    results.push(...undoResults(request, result));
  }
  return undoOutcome(undo, results);
}

/** Whether a key press is Undo (Ctrl+Z, or Cmd+Z on a Mac) meant for the page, not a text field. */
export function isUndoKey(event) {
  if (!event || event.altKey || event.shiftKey) return false;
  if (!(event.ctrlKey || event.metaKey)) return false;
  if (String(event.key ?? '').toLowerCase() !== 'z') return false;
  return !isTextField(event.target);
}
