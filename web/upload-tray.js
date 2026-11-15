// The upload tray: every file being uploaded from the library page, docked at the bottom-right
// (a bottom sheet on phones), with a header that says how far the uploads are and how long they
// should take, and a row per file with its progress, speed, time left and what can be done
// with it (Pause, Resume, Cancel, Retry, Open, and the answer when its name is taken).
//
// The uploads belong to the tray, not to the folder on screen: the library page moves between
// folders of the same library without leaving the page while they run (see library.js).
//
// The pure parts (the state machine of a file, the speed estimate with its injectable clock,
// the header's words and the steps announced) and the controller (createUploads, which runs
// the files one after another through upload.js) are tested without a page; mountUploadTray
// draws the tray. Nothing runs at import.

import { formatSize, h, pageFor } from './common.js';
import { fileIcon, fileKind } from './files.js';
import { CONFLICT_CHOICES, UploadQueue, conflictQuestion, outcomeNote } from './upload.js';

// ---- the state machine of one file ----

/** What a file in the tray can be. */
export const TRAY_STATES = Object.freeze(['queued', 'running', 'paused', 'conflict', 'done', 'skipped', 'failed', 'cancelled']);

const MOVES = Object.freeze({
  queued: { start: 'running', pause: 'paused', cancel: 'cancelled' },
  running: {
    progress: 'running',
    conflict: 'conflict',
    pause: 'paused',
    cancel: 'cancelled',
    finish: 'done',
    skip: 'skipped',
    fail: 'failed',
  },
  conflict: { answer: 'running', pause: 'paused', cancel: 'cancelled', skip: 'skipped', fail: 'failed', finish: 'done' },
  paused: { resume: 'queued', cancel: 'cancelled' },
  failed: { retry: 'queued' },
  done: {},
  skipped: {},
  cancelled: {},
});

/** The state `status` moves to on `event`, or null when that cannot happen from there. */
export function nextStatus(status, event) {
  return MOVES[status]?.[event] ?? null;
}

/** Whether a file in `status` still has work to do (it keeps the page from being left quietly). */
export const isActive = (status) => ['queued', 'running', 'paused', 'conflict'].includes(status);

/** The event that brings a file to what the upload queue reports (`outcome.status`). */
export function eventFor(queueStatus) {
  switch (queueStatus) {
    case 'hashing':
    case 'uploading':
      return 'progress';
    case 'conflict':
      return 'conflict';
    case 'done':
      return 'finish';
    case 'skipped':
      return 'skip';
    case 'failed':
      return 'fail';
    case 'paused':
      return 'pause';
    case 'cancelled':
      return 'cancel';
    default:
      return null;
  }
}

/**
 * The buttons a row offers in `status`, in order: `pause`, `resume`, `cancel`, `open`, `retry`,
 * `remove`. A conflict's own three answers are drawn with its question.
 */
export function rowActions(status, { retry = false, open = false } = {}) {
  switch (status) {
    case 'queued':
    case 'running':
      return ['pause', 'cancel'];
    case 'paused':
      return ['resume', 'cancel'];
    case 'conflict':
      return ['cancel'];
    case 'done':
      return open ? ['open', 'remove'] : ['remove'];
    case 'failed':
      return retry ? ['retry', 'remove'] : ['remove'];
    default:
      return ['remove'];
  }
}

// ---- speed and time left ----

const realNow = () => Date.now();

/**
 * Bytes per second, smoothed: each sample moves the estimate towards the speed since the last
 * one by an amount that grows with the time between them (half of the way after `halfLife`
 * milliseconds), so one slow or quick chunk does not swing it. `sample(total)` takes the bytes
 * sent so far; `rebase(total)` starts measuring again from `total` without counting the gap
 * (after a pause, or while a file is being checked); `speed()` is null until `warmup`
 * milliseconds have been measured. `now` is the clock, injectable for tests.
 */
export function createRateMeter({ now = realNow, halfLife = 4000, warmup = 1000 } = {}) {
  let last = null; // { at, bytes }
  let rate = null;
  let measured = 0;
  return {
    sample(total) {
      const at = now();
      if (!last || total < last.bytes) {
        last = { at, bytes: total };
        return;
      }
      const gap = at - last.at;
      if (gap <= 0) return;
      const instant = ((total - last.bytes) / gap) * 1000;
      const weight = 1 - 2 ** (-gap / halfLife);
      rate = rate === null ? instant : rate + weight * (instant - rate);
      measured += gap;
      last = { at, bytes: total };
    },
    rebase(total) {
      last = { at: now(), bytes: total };
    },
    speed() {
      return measured >= warmup && rate !== null ? rate : null;
    },
    /** Seconds to send `remaining` bytes at the current speed, or null when not known yet. */
    timeLeft(remaining) {
      const speed = this.speed();
      if (speed === null || speed <= 0) return null;
      return Math.max(0, remaining / speed);
    },
  };
}

const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** "about 2 minutes left" for `seconds`; '' when not known. Rounded so that it does not flicker. */
export function timeLeftText(seconds) {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds)) return '';
  if (seconds < 10) return 'a few seconds left';
  if (seconds < 60) return 'less than a minute left';
  if (seconds < 3600) {
    const minutes = seconds < 600 ? Math.max(1, Math.round(seconds / 60)) : Math.round(seconds / 300) * 5;
    if (minutes >= 60) return 'about 1 hour left';
    return `about ${plural(minutes, 'minute')} left`;
  }
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.round((seconds - hours * 3600) / 600) * 10;
  if (hours >= 5 || minutes === 0) return `about ${plural(hours, 'hour')} left`;
  if (minutes === 60) return `about ${plural(hours + 1, 'hour')} left`;
  return `about ${plural(hours, 'hour')} ${plural(minutes, 'minute')} left`;
}

/** "1.2 MB/s"; '' when not known. */
export const speedText = (bytesPerSecond) =>
  bytesPerSecond === null || bytesPerSecond === undefined || !Number.isFinite(bytesPerSecond) ? '' : `${formatSize(Math.round(bytesPerSecond))}/s`;

// ---- the header and the announcements ----

const sizeOf = (entry) => entry.size ?? 0;

/** How far all the uploads are, 0 to 100 (cancelled files left out, skipped ones counted done). */
export function overallPercent(entries) {
  let size = 0;
  let sent = 0;
  for (const entry of entries) {
    if (entry.status === 'cancelled') continue;
    size += sizeOf(entry);
    sent += ['done', 'skipped'].includes(entry.status) ? sizeOf(entry) : Math.min(entry.sent ?? 0, sizeOf(entry));
  }
  if (size === 0) return entries.some((entry) => isActive(entry.status)) ? 0 : 100;
  return Math.floor((sent / size) * 100);
}

/** The counts of `entries` by state. */
export function countStates(entries) {
  const counts = Object.fromEntries(TRAY_STATES.map((state) => [state, 0]));
  for (const entry of entries) counts[entry.status] = (counts[entry.status] ?? 0) + 1;
  return counts;
}

/**
 * What the tray's header says: `{ title, finished, failed }`. `state` is `{ entries,
 * secondsLeft }`, each entry `{ status, destination: { name } }`. While uploads run, "Uploading
 * 3 of 5 — about 2 minutes left" (or what holds them up); when all have finished, "5 files
 * uploaded to Documents", or how many failed.
 */
export function trayHeader({ entries = [], secondsLeft = null } = {}) {
  const counts = countStates(entries);
  const total = entries.length - counts.cancelled;
  const active = entries.filter((entry) => isActive(entry.status)).length;
  const finished = counts.done + counts.skipped + counts.failed;
  if (entries.length === 0) return { title: '', finished: true, failed: 0 };
  if (active > 0) {
    let title;
    if (counts.running + counts.conflict + counts.queued === 0) {
      title = `Paused — ${counts.done} of ${total} uploaded`;
    } else {
      const position = Math.min(total, finished + 1);
      title = `Uploading ${position} of ${total}`;
      let rest = '';
      if (counts.conflict > 0) rest = counts.conflict === 1 ? '1 file needs an answer' : `${counts.conflict} files need an answer`;
      else if (counts.running === 0) rest = 'starting the next file';
      else rest = timeLeftText(secondsLeft);
      if (rest) title += ` — ${rest}`;
    }
    return { title, finished: false, failed: counts.failed };
  }
  const places = new Set(entries.filter((entry) => entry.status === 'done').map((entry) => entry.destination?.name ?? ''));
  const where = places.size === 1 && [...places][0] ? ` to ${[...places][0]}` : '';
  let title;
  if (counts.failed > 0) {
    title = `${counts.done} of ${total} uploaded — ${counts.failed} failed`;
  } else if (counts.done > 0) {
    title = `${plural(counts.done, 'file')} uploaded${where}`;
  } else if (counts.skipped > 0) {
    title = `No files uploaded — ${counts.skipped} skipped`;
  } else {
    title = 'Uploads cancelled';
  }
  const extra = [];
  if (counts.failed === 0 && counts.done > 0 && counts.skipped > 0) extra.push(`${counts.skipped} skipped`);
  if (counts.cancelled > 0 && total > 0) extra.push(`${counts.cancelled} cancelled`);
  if (extra.length) title += ` (${extra.join(', ')})`;
  return { title, finished: true, failed: counts.failed };
}

/** The steps of the overall progress that are announced. */
export const ANNOUNCED_STEPS = Object.freeze([25, 50, 75, 100]);

/**
 * Whether `percent` reaches a step not announced yet after `last` (the last step announced):
 * `{ step, say }`. When more files are added and the percentage falls, the step falls with it,
 * silently, so the next step is announced again.
 */
export function announceStep(last, percent) {
  const step = ANNOUNCED_STEPS.filter((s) => percent >= s).at(-1) ?? 0;
  if (step > last) return { step, say: true };
  return { step: Math.min(last, step), say: false };
}

/** The bytes line of a row: "3.0 MB of 20.0 MB · 1.2 MB/s · about 1 minute left". */
export function rowStats(entry, { speed = null, secondsLeft = null } = {}) {
  const size = formatSize(sizeOf(entry));
  switch (entry.status) {
    case 'queued':
      return entry.sent > 0 ? `${formatSize(entry.sent)} of ${size} · waiting` : `${size} · waiting`;
    case 'paused':
      return `${formatSize(entry.sent ?? 0)} of ${size} · paused`;
    case 'running':
      if (entry.phase === 'hashing') return `Checking the file… ${formatSize(entry.hashed ?? 0)} of ${size}`;
      return [`${formatSize(entry.sent ?? 0)} of ${size}`, speedText(speed), timeLeftText(secondsLeft)].filter(Boolean).join(' · ');
    case 'conflict':
      return `${size} · waiting for your answer`;
    case 'done':
      return size;
    case 'cancelled':
      return 'Cancelled';
    default:
      return '';
  }
}

// ---- the controller ----

let lastId = 0;

/**
 * The files in the tray, uploaded one after another. `add(files, destination)` queues files
 * (as upload.js takes them) for the folder `destination` (`{ id, name }`); `pause`, `resume`,
 * `cancel`, `retry`, `answer` and `remove` act on an entry. `onChange(entry)` is told whenever
 * an entry changes (with null when entries come or go); `onIdle()` when the last file stops.
 *
 * Options: `site` (for explaining a quota refusal), `now` (the clock), `makeQueue` (makes an
 * upload.js queue from its options; tests pass a stub) and `release(sessionId)` (gives a kept
 * upload session back to the server).
 */
export function createUploads({
  site = null,
  now = realNow,
  makeQueue = (options) => new UploadQueue(options),
  release = () => {},
  onChange = () => {},
  onIdle = () => {},
} = {}) {
  const entries = [];
  const overall = createRateMeter({ now });
  let running = null;
  let rest = null; // the conflict answer chosen "for all"

  const changed = (entry) => onChange(entry);
  const sentNow = () => entries.reduce((sum, entry) => sum + (entry.status === 'cancelled' ? 0 : Math.min(entry.sent ?? 0, entry.size ?? 0)), 0);

  function move(entry, event) {
    const to = nextStatus(entry.status, event);
    if (!to) return false;
    entry.status = to;
    return true;
  }

  /** Copies what the queue reports into the entry, moving its state when the queue's moved. */
  function follow(entry, outcome) {
    const event = eventFor(outcome.status);
    if (event === 'progress' && entry.status === 'conflict') move(entry, 'answer');
    else if (event && event !== 'progress') move(entry, event);
    entry.phase = outcome.status === 'hashing' ? 'hashing' : 'sending';
    if (outcome.status === 'hashing') {
      entry.hashed = outcome.hashed ?? 0;
    } else if (typeof outcome.sent === 'number') {
      const was = entry.sent;
      entry.sent = outcome.sent;
      if (entry.status === 'running') {
        if (entry.sending) {
          entry.meter.sample(entry.sent);
          overall.sample(sentNow());
        } else {
          // The first bytes: measure from here, so that checking the file is not counted as slow.
          entry.sending = true;
          entry.meter.rebase(entry.sent);
          overall.rebase(sentNow());
        }
      }
      if (entry.sent < was) entry.meter.rebase(entry.sent);
    }
    entry.name = outcome.name ?? entry.name;
    entry.message = outcome.message ?? '';
    entry.suggested = outcome.suggested ?? entry.suggested ?? '';
    changed(entry);
  }

  function batchFor(destination) {
    const batch = { destination, queue: null };
    batch.queue = makeQueue({
      parent: destination.id,
      site,
      checkScans: true,
      onChange: (outcome) => {
        const entry = entries.find((e) => e.outcome === outcome);
        if (entry) follow(entry, outcome);
      },
      resolve: ({ outcome }) => {
        if (rest) return Promise.resolve({ choice: rest, all: false });
        const entry = entries.find((e) => e.outcome === outcome);
        return new Promise((resolve) => {
          if (!entry) return resolve({ choice: 'skip', all: false });
          entry.answerWith = resolve;
        });
      },
    });
    return batch;
  }

  function add(files, destination) {
    if (!files.length) return [];
    const batch = batchFor(destination);
    const added = files.map((file) => ({
      id: (lastId += 1),
      batch,
      file,
      outcome: UploadQueue.outcomeFor(file),
      name: file.name,
      size: file.size,
      sent: 0,
      hashed: 0,
      status: 'queued',
      phase: 'waiting',
      message: '',
      destination,
      folders: file.path ?? [],
      meter: createRateMeter({ now }),
      sending: false,
      wanted: null,
    }));
    entries.push(...added);
    changed(null);
    pump();
    return added;
  }

  async function pump() {
    if (running) return;
    const entry = entries.find((e) => e.status === 'queued');
    if (!entry) {
      onIdle();
      return;
    }
    running = entry;
    move(entry, 'start');
    entry.sending = false;
    entry.wanted = null;
    changed(entry);
    const outcome = entry.outcome;
    try {
      await entry.batch.queue.runOne(outcome);
    } catch {
      // runOne never rejects; a stub might
      outcome.status = 'failed';
    }
    running = null;
    settle(entry, outcome);
    pump();
  }

  /** What the run of an entry ended as, and what the person asked for meanwhile. */
  function settle(entry, outcome) {
    entry.answerWith = null;
    entry.keep = Boolean(outcome.keep);
    entry.sessionId = outcome.sessionId;
    entry.retry = Boolean(outcome.retry);
    entry.docId = outcome.docId ?? null;
    entry.editor = outcome.editor ?? null;
    entry.note = outcome.status === 'done' ? outcomeNote(outcome) : '';
    entry.name = outcome.name ?? entry.name;
    entry.message = outcome.message ?? '';
    const final = { done: 'done', skipped: 'skipped', failed: 'failed', paused: 'paused', cancelled: 'cancelled' }[outcome.status] ?? 'failed';
    if (final === 'paused' && entry.wanted === 'cancel') {
      // Paused, then cancelled before the pause had landed: give the session back.
      if (entry.keep && entry.sessionId) release(entry.sessionId);
      entry.keep = false;
      entry.status = 'cancelled';
    } else {
      entry.status = final;
    }
    if (entry.status === 'done') entry.sent = entry.size;
    if (entry.status === 'paused' && entry.wanted === 'resume') {
      // Resumed before the pause had landed: back in the queue.
      again(entry);
      move(entry, 'resume');
    }
    changed(entry);
  }

  const find = (id) => entries.find((entry) => entry.id === id) ?? null;

  /** The file to run again: a kept session only needs the rest sent; otherwise from the start. */
  function again(entry) {
    const file = entry.keep && entry.sessionId
      ? { ...entry.file, resumeId: entry.sessionId, resumeChoice: entry.outcome.choice ?? null, parentId: entry.outcome.parentId }
      : entry.file;
    entry.outcome = UploadQueue.outcomeFor(file);
    entry.outcome.sent = entry.keep ? entry.sent : 0;
    if (!entry.keep) entry.sent = 0;
    entry.keep = false;
  }

  return {
    entries,
    add,
    find,
    /** Pauses a file: one waiting is held back; one running stops after keeping what was sent. */
    pause(id) {
      const entry = find(id);
      if (!entry) return;
      if (entry.status === 'queued') {
        move(entry, 'pause');
        changed(entry);
      } else if (entry.status === 'running' || entry.status === 'conflict') {
        entry.wanted = 'pause';
        move(entry, 'pause');
        entry.batch.queue.halt(entry.outcome, 'pause');
        changed(entry);
      }
    },
    /** Puts a paused file back in the queue; it carries on from what the server holds. */
    resume(id) {
      const entry = find(id);
      if (!entry || entry.status !== 'paused') return;
      if (running === entry) {
        entry.wanted = 'resume'; // the pause has not landed yet; settle puts it back in the queue
        return;
      }
      again(entry);
      move(entry, 'resume');
      changed(entry);
      pump();
    },
    /** Cancels a file: nothing more is sent and an open session is given back. */
    cancel(id) {
      const entry = find(id);
      if (!entry) return;
      if (running === entry) {
        entry.wanted = 'cancel';
        entry.batch.queue.halt(entry.outcome, 'cancel');
        entry.answerWith?.({ choice: null, all: false });
        if (entry.status !== 'paused') move(entry, 'cancel');
        else entry.status = 'cancelled';
        changed(entry);
        return;
      }
      if (!move(entry, 'cancel')) return;
      if (entry.keep && entry.sessionId) release(entry.sessionId);
      entry.keep = false;
      changed(entry);
      if (!entries.some((e) => isActive(e.status))) onIdle();
    },
    /** Uploads a failed file again. */
    retry(id) {
      const entry = find(id);
      if (!entry || entry.status !== 'failed' || !entry.retry) return;
      again(entry);
      move(entry, 'retry');
      entry.message = '';
      changed(entry);
      pump();
    },
    /** Answers the name question of a file: `choice` replace, keep_both or skip, `all` for the rest too. */
    answer(id, choice, all = false) {
      const entry = find(id);
      if (!entry || entry.status !== 'conflict' || !entry.answerWith) return;
      if (all) rest = choice;
      const answer = entry.answerWith;
      entry.answerWith = null;
      move(entry, 'answer');
      changed(entry);
      answer({ choice, all: false });
    },
    /** Takes a file that has stopped out of the tray (a kept session is given back). */
    remove(id) {
      const at = entries.findIndex((entry) => entry.id === id);
      if (at < 0 || isActive(entries[at].status)) return;
      const [entry] = entries.splice(at, 1);
      if (entry.keep && entry.sessionId) release(entry.sessionId);
      changed(null);
    },
    /** Takes every file that has stopped out of the tray; the "for all" answer is forgotten. */
    clear() {
      for (const entry of [...entries]) if (!isActive(entry.status)) this.remove(entry.id);
      if (entries.length === 0) rest = null;
      changed(null);
    },
    /** Cancels every file still to do. */
    cancelAll() {
      for (const entry of [...entries]) if (isActive(entry.status)) this.cancel(entry.id);
    },
    active: () => entries.filter((entry) => isActive(entry.status)).length,
    /** The overall speed in bytes per second, or null. */
    speed: () => (running?.status === 'running' && running.sending ? overall.speed() : null),
    /** Seconds left for every file still to do, or null when not known. */
    secondsLeft() {
      if (!running || running.status !== 'running' || !running.sending) return null;
      const remaining = entries
        .filter((entry) => ['queued', 'running'].includes(entry.status))
        .reduce((sum, entry) => sum + Math.max(0, (entry.size ?? 0) - (entry.sent ?? 0)), 0);
      return overall.timeLeft(remaining);
    },
    /** Speed and time left of one file: `{ speed, secondsLeft }`. */
    pace(entry) {
      if (entry !== running || entry.status !== 'running' || !entry.sending) return { speed: null, secondsLeft: null };
      return { speed: entry.meter.speed(), secondsLeft: entry.meter.timeLeft(Math.max(0, entry.size - entry.sent)) };
    },
  };
}

// ---- the tray on the page ----

const SUMMARY_TIME = 10_000; // how long a summary of uploads that all went well stays

/**
 * Draws `uploads` (from createUploads) in a tray appended to the page. `openFolder(id)` opens a
 * destination folder from the summary link; `confirm(text, label)` asks before cancelling
 * uploads (common.js confirmDialog). Answers `{ changed(entry), idle(), element }`: the page
 * passes the controller's onChange and onIdle to these.
 */
export function mountUploadTray({ uploads, openFolder = null, confirm = async () => true }) {
  const title = h('h2', { class: 'tray-title', id: 'tray-title', tabindex: '-1' });
  const bar = h('progress', { class: 'tray-progress', max: '100', value: '0', 'aria-label': 'All uploads' });
  const toggle = h('button', { type: 'button', class: 'plain tray-toggle', 'aria-expanded': 'true', 'aria-controls': 'tray-list' }, 'Minimise');
  const close = h('button', { type: 'button', class: 'plain tray-close', 'aria-label': 'Close uploads' }, '×');
  const summaryLink = h('a', { class: 'tray-link', href: '#', hidden: true });
  const list = h('ol', { class: 'tray-list', id: 'tray-list' });
  const live = h('p', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
  const tray = h(
    'section',
    { class: 'upload-tray', 'aria-labelledby': 'tray-title', hidden: true },
    h('div', { class: 'tray-head' }, h('div', { class: 'tray-heading' }, title, bar, summaryLink), toggle, close),
    list,
    live,
  );
  document.body.append(tray);

  const rows = new Map(); // entry -> { item, key, bar, stats }
  let expanded = true;
  let summary = false; // all finished: the tray is a short note with a link
  let lastStep = 0;
  let scheduled = false;
  let timer = 0;
  let held = false; // pointer or focus in the tray: the summary waits

  const say = (text) => {
    live.textContent = '';
    setTimeout(() => {
      live.textContent = text;
    }, 50);
  };

  function setExpanded(open) {
    expanded = open;
    list.hidden = !open;
    toggle.setAttribute('aria-expanded', String(open));
    toggle.textContent = summary ? (open ? 'Hide files' : 'Show files') : open ? 'Minimise' : 'Expand';
    tray.classList.toggle('minimised', !open);
  }
  toggle.addEventListener('click', () => {
    setExpanded(!expanded);
    stopTimer();
    if (summary && !expanded) startTimer();
  });

  close.addEventListener('click', async () => {
    const active = uploads.active();
    if (active > 0) {
      const yes = await confirm(
        `Stop ${active === 1 ? 'the upload that has' : `the ${active} uploads that have`} not finished? Files already uploaded stay.`,
        'Stop uploads',
      );
      if (!yes) return;
      uploads.cancelAll();
    }
    hide();
  });

  function hide() {
    stopTimer();
    uploads.clear();
    rows.clear();
    list.replaceChildren();
    summary = false;
    summaryLink.hidden = true;
    lastStep = 0;
    const hadFocus = tray.contains(document.activeElement);
    tray.hidden = true;
    if (hadFocus) document.querySelector('h1')?.focus?.();
  }

  // A summary leaves after a while, unless the pointer or focus is on it.
  function startTimer() {
    stopTimer();
    if (held) return;
    timer = setTimeout(() => {
      if (summary && !uploads.active()) hide();
    }, SUMMARY_TIME);
  }
  function stopTimer() {
    clearTimeout(timer);
    timer = 0;
  }
  const hold = () => {
    held = true;
    stopTimer();
  };
  const release = () => {
    held = false;
    if (summary && !expanded && !uploads.entries.some((e) => e.status === 'failed')) startTimer();
  };
  tray.addEventListener('mouseenter', hold);
  tray.addEventListener('mouseleave', () => {
    if (!tray.contains(document.activeElement)) release();
  });
  tray.addEventListener('focusin', hold);
  tray.addEventListener('focusout', (event) => {
    if (!tray.contains(event.relatedTarget) && !tray.matches(':hover')) release();
  });

  summaryLink.addEventListener('click', (event) => {
    const id = summaryLink.dataset.id;
    if (!openFolder || !id || event.ctrlKey || event.metaKey || event.shiftKey || event.button !== 0) return;
    event.preventDefault();
    openFolder(id);
  });

  function schedule() {
    if (scheduled) return;
    scheduled = true;
    const later = globalThis.requestAnimationFrame ?? ((fn) => setTimeout(fn, 50));
    later(() => {
      scheduled = false;
      draw();
    });
  }

  /** What decides whether a row is built again: everything but the bytes. */
  const keyOf = (entry) => `${entry.status}|${entry.phase === 'hashing'}|${entry.name}|${entry.message}|${entry.retry}|${entry.docId}`;

  function draw() {
    const entries = uploads.entries;
    if (entries.length === 0) {
      tray.hidden = true;
      return;
    }
    if (tray.hidden) {
      tray.hidden = false;
      summary = false;
      // A phone starts with the sheet folded to its header, so the page stays usable.
      setExpanded(!globalThis.matchMedia?.('(max-width: 760px)').matches);
    }
    // Rows that went.
    for (const [entry, row] of rows) {
      if (entries.includes(entry)) continue;
      const hadFocus = row.item.contains(document.activeElement);
      row.item.remove();
      rows.delete(entry);
      if (hadFocus) (list.querySelector('button') ?? toggle).focus();
    }
    entries.forEach((entry, at) => {
      let row = rows.get(entry);
      if (!row || row.key !== keyOf(entry)) {
        const made = rowFor(entry);
        if (row) {
          const focused = row.item.contains(document.activeElement) ? document.activeElement?.dataset?.act ?? '' : null;
          row.item.replaceWith(made.item);
          if (focused !== null) {
            const again = (focused && made.item.querySelector(`[data-act="${focused}"]`)) || made.item.querySelector('button, a');
            (again ?? toggle).focus();
          }
        }
        row = made;
        rows.set(entry, row);
      }
      if (list.children[at] !== row.item) list.insertBefore(row.item, list.children[at] ?? null);
      if (row.bar) {
        row.bar.max = Math.max(1, entry.size);
        row.bar.value = entry.phase === 'hashing' && entry.status === 'running' ? (entry.hashed ?? 0) : Math.min(entry.sent ?? 0, entry.size);
      }
      if (row.stats) row.stats.textContent = rowStats(entry, uploads.pace(entry));
    });
    const header = trayHeader({ entries, secondsLeft: uploads.secondsLeft() });
    if (title.textContent !== header.title) title.textContent = header.title;
    const percent = overallPercent(entries);
    bar.value = percent;
    bar.setAttribute('aria-valuetext', `${percent}%`);
    tray.classList.toggle('has-failed', header.failed > 0);
    if (!header.finished) {
      const step = announceStep(lastStep, percent);
      lastStep = step.step;
      if (step.say && step.step < 100) say(`Uploads ${step.step}% done.`);
      summaryLink.hidden = true;
      if (summary) {
        summary = false;
        stopTimer();
        setExpanded(true);
      }
    }
  }

  /** All files stopped: say so, and fold the tray into a summary when nothing failed. */
  function idle() {
    draw();
    const entries = uploads.entries;
    if (entries.length === 0 || uploads.active() > 0) return;
    const header = trayHeader({ entries });
    lastStep = 0;
    say(header.title);
    const done = entries.filter((entry) => entry.status === 'done');
    const places = [...new Set(done.map((entry) => entry.destination.id))];
    if (places.length === 1) {
      const destination = done[0].destination;
      summaryLink.textContent = `Open ${destination.name}`;
      summaryLink.href = pageFor('folder', destination.id);
      summaryLink.dataset.id = destination.id;
      summaryLink.hidden = false;
    } else {
      summaryLink.hidden = true;
    }
    summary = true;
    if (header.failed > 0) {
      setExpanded(true);
      return;
    }
    if (!tray.contains(document.activeElement)) setExpanded(false);
    else toggle.textContent = expanded ? 'Hide files' : 'Show files';
    startTimer();
  }

  function act(entry, action) {
    if (action === 'pause') uploads.pause(entry.id);
    else if (action === 'resume') uploads.resume(entry.id);
    else if (action === 'cancel') uploads.cancel(entry.id);
    else if (action === 'retry') uploads.retry(entry.id);
    else if (action === 'remove') {
      uploads.remove(entry.id);
      if (uploads.entries.length === 0) hide();
    }
  }

  const LABELS = { pause: 'Pause', resume: 'Resume', cancel: 'Cancel', retry: 'Retry', remove: 'Remove' };

  function rowFor(entry) {
    const kind = fileKind(entry.name);
    const icon =
      entry.status === 'done'
        ? h('span', { class: 'tray-icon tray-tick', 'aria-hidden': 'true' }, '✓')
        : entry.status === 'failed'
          ? h('span', { class: 'tray-icon tray-bad', 'aria-hidden': 'true' }, '!')
          : h('span', { class: 'tray-icon', 'aria-hidden': 'true' }, fileIcon(kind));
    const where = [entry.destination?.name ?? '', ...(entry.folders ?? [])].filter(Boolean).join(' / ');
    const stateWords = { done: 'Uploaded', failed: 'Not uploaded', skipped: 'Skipped', cancelled: 'Cancelled', paused: 'Paused', conflict: 'Name taken' }[entry.status];
    const main = h(
      'div',
      { class: 'tray-main' },
      h('p', { class: 'tray-name', title: entry.name }, entry.name, stateWords ? h('span', { class: 'visually-hidden' }, `, ${stateWords}`) : null),
      h('p', { class: 'tray-where' }, where ? `to ${where}` : ''),
    );
    let bar = null;
    let stats = null;
    if (['queued', 'running', 'paused'].includes(entry.status)) {
      bar = h('progress', { max: String(Math.max(1, entry.size)), value: '0', 'aria-label': `Upload of ${entry.name}` });
      main.append(bar);
    }
    if (entry.status !== 'failed' && entry.status !== 'skipped') {
      stats = h('p', { class: 'tray-stats' });
      main.append(stats);
    }
    if (entry.status === 'done' && entry.note && entry.note !== 'Uploaded.') main.append(h('p', { class: 'tray-note' }, entry.note));
    if (entry.status === 'skipped') main.append(h('p', { class: 'tray-note' }, 'Skipped: a file with that name was already there.'));
    if (entry.status === 'failed') main.append(h('p', { class: 'tray-error' }, entry.message || 'This file was not uploaded.'));
    if (entry.status === 'conflict') main.append(conflictBox(entry));
    const actions = h('div', { class: 'tray-actions' });
    for (const action of rowActions(entry.status, { retry: entry.retry, open: Boolean(entry.docId) })) {
      if (action === 'open') {
        actions.append(
          h('a', { class: 'button plain', href: pageFor('document', entry.docId, entry.editor ?? undefined), 'data-act': 'open', 'aria-label': `Open ${entry.name}` }, 'Open'),
        );
        continue;
      }
      const button = h('button', { type: 'button', class: 'plain', 'data-act': action, 'aria-label': `${LABELS[action]} ${entry.name}` }, LABELS[action]);
      button.addEventListener('click', () => act(entry, action));
      actions.append(button);
    }
    const item = h('li', { class: 'tray-row', 'data-status': entry.status }, icon, main, actions);
    return { item, key: keyOf(entry), bar, stats };
  }

  function conflictBox(entry) {
    const id = `tray-all-${entry.id}`;
    const all = h('input', { type: 'checkbox', id });
    const others = uploads.entries.filter((e) => e !== entry && ['queued', 'paused'].includes(e.status)).length;
    const buttons = CONFLICT_CHOICES.map(({ value, label }) => {
      const button = h('button', { type: 'button', class: value === 'replace' ? '' : 'plain', 'data-act': value }, label);
      button.addEventListener('click', () => uploads.answer(entry.id, value, all.checked));
      return button;
    });
    return h(
      'div',
      { class: 'tray-conflict', role: 'group', 'aria-label': `${entry.name}: name taken` },
      h('p', {}, conflictQuestion(entry.name, entry.suggested || '')),
      h('div', { class: 'tray-choices' }, ...buttons),
      others > 0 ? h('label', { class: 'check', for: id }, all, ' Apply to all') : null,
    );
  }

  return {
    element: tray,
    changed(entry) {
      if (entry?.status === 'conflict' && !rows.get(entry)?.key?.startsWith('conflict')) {
        say(`${entry.name}: a file with that name is already here. Choose Replace, Keep both or Skip in the uploads tray.`);
        if (!expanded) setExpanded(true);
      }
      schedule();
    },
    idle,
  };
}
