// The library's keyboard selection, as pure functions: where the current row moves for a key,
// how Shift+arrow extends the selection from an anchor, how Space toggles a row and what is
// announced. The page (library.js) keeps the state and draws it.

/**
 * The row a key moves the current row to, from `at` among `count` rows: Up and Down one row
 * (no wrapping), Home and End to the ends, Page Up and Page Down `page` rows. Null when the key
 * does not move or there are no rows. With no current row (`at` -1) Down and Home go to the
 * first, Up and End to the last.
 */
export function moveIndex(key, at, count, page = 10) {
  if (count <= 0) return null;
  const last = count - 1;
  const from = at >= 0 && at <= last ? at : -1;
  const step = Math.max(1, Math.floor(page) || 1);
  switch (key) {
    case 'ArrowDown':
      return from === -1 ? 0 : Math.min(last, from + 1);
    case 'ArrowUp':
      return from === -1 ? last : Math.max(0, from - 1);
    case 'Home':
      return 0;
    case 'End':
      return last;
    case 'PageDown':
      return from === -1 ? Math.min(last, step - 1) : Math.min(last, from + step);
    case 'PageUp':
      return from === -1 ? 0 : Math.max(0, from - step);
    default:
      return null;
  }
}

/**
 * The selection after Shift+arrow: `base` (the selection when the anchor was set) plus every
 * selectable row between `anchor` and `to` (ids, in the order `order` shows them). Moving back
 * towards the anchor shrinks the range again; rows selected before stay selected. `selectable`
 * says which ids may be selected (a library may not).
 */
export function extendSelection(order, base, anchor, to, selectable = () => true) {
  const result = new Set(base);
  let from = order.indexOf(anchor);
  const end = order.indexOf(to);
  if (end === -1) return result;
  if (from === -1) from = end;
  const [low, high] = from <= end ? [from, end] : [end, from];
  for (let index = low; index <= high; index += 1) {
    if (selectable(order[index])) result.add(order[index]);
  }
  return result;
}

/** The selection with `id` checked or unchecked (a new Set). */
export function toggleSelection(selected, id) {
  const result = new Set(selected);
  if (result.has(id)) result.delete(id);
  else result.add(id);
  return result;
}

/** What is said when the selection changes: "3 selected", "1 selected", "Nothing selected". */
export function selectionText(count) {
  return count === 0 ? 'Nothing selected' : `${count} selected`;
}
