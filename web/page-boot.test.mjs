// Page boot order: a page script must not run code that can reach a
// module-level `const`, `let` or `class` binding before that binding's line.
//
// A page script (one loaded by a `<script src>` in web/*.html) runs its
// top-level statements in order. When a top-level statement calls a function
// that reads a `const`/`let`/`class` binding declared further down, the browser
// throws "Cannot access 'X' before initialization" and the page stops drawing.
// The same happens when an event handler registered early fires while the page
// waits on a top-level `await`.
//
// The rule this test enforces:
//
//   For every top-level statement that runs code (anything but a `function`
//   declaration, a `class` declaration, an `import`, or a `const` whose value
//   is an arrow function or function expression), take the names it mentions;
//   follow every name that is a top-level function (declaration or const
//   arrow) into that function's body, and on through the names that body
//   mentions. None of the names reached may be a top-level `const`, `let` or
//   `class` declared on a later line.
//
// So module-level constants and state go above the code that starts the page;
// a binding that can only be filled once the page is running (the signed-in
// user, say) is declared up top with `let name;` and assigned later.
//
// The check is by name: it does not track scopes, so a local variable that
// shares a name with a late top-level binding also counts (rename one of them).
// It reads the scripts by their formatting: a top-level statement starts at
// column 0 of a line that begins outside any bracket, string, template literal
// or comment.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const WEB = dirname(fileURLToPath(import.meta.url));

/** The scripts the pages load, by file name. */
function pageScripts() {
  const scripts = new Set();
  for (const page of readdirSync(WEB).filter((name) => name.endsWith('.html'))) {
    const html = readFileSync(join(WEB, page), 'utf8');
    for (const match of html.matchAll(/<script\b[^>]*\bsrc="\.?\/?([^"]+\.js)"/g)) scripts.add(match[1]);
  }
  return [...scripts].sort();
}

const REGEX_AFTER_WORDS = new Set(['return', 'typeof', 'instanceof', 'in', 'of', 'new', 'delete', 'void', 'throw', 'case', 'do', 'else', 'yield', 'await']);

/**
 * The top-level statements of `source`, each with its 1-based line, its first
 * line of text, and the names it mentions (identifiers outside strings,
 * comments and regular expressions, not after a `.`, and not object keys).
 */
export function topLevelStatements(source) {
  const statements = [];
  // Open contexts: '{', '(', '[', '`' (a template literal) or '${'.
  const stack = [];
  let i = 0;
  let line = 1;
  let lineStart = true;
  let last = ''; // the last significant character, or 'w' after a word
  let lastKind = ''; // 'value' when a '/' next is a division
  let pendingWord = null; // a word whose following character decides if it is an object key
  const n = source.length;
  const inTemplate = () => stack[stack.length - 1] === '`';
  const current = () => statements[statements.length - 1];
  const flushWord = (next) => {
    if (pendingWord && current()) {
      const isKey = next === ':' && (pendingWord.before === '{' || pendingWord.before === ',') && pendingWord.ternaryDepth === 0;
      if (!isKey) current().names.add(pendingWord.word);
    }
    pendingWord = null;
  };
  let ternaries = 0; // open `?` at this point, so `a ? b : c` keeps `b`
  while (i < n) {
    const c = source[i];
    if (lineStart && !inTemplate()) {
      lineStart = false;
      if (stack.length === 0 && /[A-Za-z_$({[!+\-`'"]/.test(c)) {
        flushWord('');
        const end = source.indexOf('\n', i);
        statements.push({ line, text: source.slice(i, end === -1 ? n : end).trim(), names: new Set(), start: i });
        ternaries = 0;
      }
    }
    if (c === '\n') {
      line += 1;
      lineStart = true;
      i += 1;
      continue;
    }
    if (inTemplate()) {
      if (c === '\\') i += 2;
      else if (c === '`') {
        stack.pop();
        lastKind = 'value';
        last = '`';
        i += 1;
      } else if (c === '$' && source[i + 1] === '{') {
        stack.push('${');
        lastKind = 'punct';
        last = '{';
        i += 2;
      } else i += 1;
      continue;
    }
    if (/\s/.test(c)) {
      i += 1;
      continue;
    }
    if (c === '/' && source[i + 1] === '/') {
      while (i < n && source[i] !== '\n') i += 1;
      continue;
    }
    if (c === '/' && source[i + 1] === '*') {
      const end = source.indexOf('*/', i + 2);
      const stop = end === -1 ? n : end + 2;
      for (let j = i; j < stop; j += 1) if (source[j] === '\n') line += 1;
      i = stop;
      continue;
    }
    flushWord(c);
    if (c === '"' || c === "'") {
      i += 1;
      while (i < n && source[i] !== c && source[i] !== '\n') i += source[i] === '\\' ? 2 : 1;
      i += 1;
      lastKind = 'value';
      last = c;
      continue;
    }
    if (c === '`') {
      stack.push('`');
      i += 1;
      continue;
    }
    if (c === '/') {
      if (lastKind === 'value') {
        lastKind = 'punct';
        last = '/';
        i += 1;
        continue;
      }
      // A regular expression literal.
      i += 1;
      let inClass = false;
      while (i < n && source[i] !== '\n') {
        const d = source[i];
        if (d === '\\') i += 2;
        else {
          if (d === '[') inClass = true;
          else if (d === ']') inClass = false;
          else if (d === '/' && !inClass) break;
          i += 1;
        }
      }
      i += 1;
      while (i < n && /[a-z]/.test(source[i])) i += 1;
      lastKind = 'value';
      last = '/';
      continue;
    }
    if (c === '{' || c === '(' || c === '[') {
      stack.push(c);
      lastKind = 'punct';
      last = c;
      i += 1;
      continue;
    }
    if (c === '}' || c === ')' || c === ']') {
      stack.pop();
      lastKind = c === '}' ? 'punct' : 'value';
      last = c;
      i += 1;
      continue;
    }
    if (/[A-Za-z_$]/.test(c)) {
      let j = i;
      while (j < n && /[A-Za-z0-9_$]/.test(source[j])) j += 1;
      const word = source.slice(i, j);
      const afterDot = last === '.' && source[i - 1] === '.' && source[i - 2] !== '.'; // not a spread
      if (!afterDot) pendingWord = { word, before: last, ternaryDepth: ternaries };
      lastKind = REGEX_AFTER_WORDS.has(word) ? 'punct' : 'value';
      last = 'w';
      i = j;
      continue;
    }
    if (/[0-9]/.test(c)) {
      while (i < n && /[0-9A-Za-z_.]/.test(source[i])) i += 1;
      lastKind = 'value';
      last = '0';
      continue;
    }
    if (c === '?' && source[i + 1] !== '.' && source[i + 1] !== '?') ternaries += 1;
    if (c === ':' && ternaries > 0) ternaries -= 1;
    if (c === '?' && (source[i + 1] === '.' || source[i + 1] === '?')) {
      last = source[i + 1] === '.' ? '.' : '?';
      lastKind = 'punct';
      i += 2;
      continue;
    }
    lastKind = 'punct';
    last = c;
    i += 1;
  }
  flushWord('');
  return statements;
}

const FUNCTION_DECLARATION = /^(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)/;
const CLASS_DECLARATION = /^(?:export\s+(?:default\s+)?)?class\s+([A-Za-z_$][\w$]*)/;
const ARROW_CONST = /^(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s+)?(?:function\b|\([^()]*(?:\([^()]*\)[^()]*)*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)/;
const BINDING = /^(?:export\s+)?(const|let|var)\s+/;

/** The names a `const`/`let` statement declares (plain names and destructuring patterns). */
function declaredNames(statement, source) {
  const head = statement.text.match(BINDING);
  if (!head) return [];
  // Read from the keyword to the first `=` or `;` outside the pattern's brackets.
  let i = statement.start + head[0].length;
  let depth = 0;
  let pattern = '';
  while (i < source.length) {
    const c = source[i];
    if ('{['.includes(c)) depth += 1;
    if ('}]'.includes(c)) depth -= 1;
    if (depth === 0 && (c === '=' || c === ';' || c === '\n')) break;
    pattern += c;
    i += 1;
  }
  // Drop `key:` in `{ key: name }`, then keep the names.
  return [...pattern.replace(/[A-Za-z_$][\w$]*\s*:/g, '').matchAll(/[A-Za-z_$][\w$]*/g)].map((m) => m[0]);
}

/**
 * The rule's breaches in `source`: each statement that runs code, and a
 * binding declared on a later line that it can reach.
 */
export function bootOrderBreaches(source) {
  const statements = topLevelStatements(source);
  const declaredAt = new Map(); // late-checkable binding name -> statement index
  const functions = new Map(); // function name -> the names its body mentions
  statements.forEach((statement, index) => {
    statement.index = index;
    const fn = statement.text.match(FUNCTION_DECLARATION);
    const cls = statement.text.match(CLASS_DECLARATION);
    const arrow = statement.text.match(ARROW_CONST);
    if (fn) {
      functions.set(fn[1], statement.names);
      statement.defines = true;
    } else if (cls) {
      declaredAt.set(cls[1], index);
      functions.set(cls[1], statement.names);
      statement.defines = true;
    } else if (arrow) {
      declaredAt.set(arrow[1], index);
      functions.set(arrow[1], statement.names);
      statement.defines = true;
    } else if (/^import\b/.test(statement.text)) {
      statement.defines = true;
    } else {
      for (const name of declaredNames(statement, source)) declaredAt.set(name, index);
    }
  });
  const reachCache = new Map();
  /** Every name reachable from `names` through top-level functions, with the function path. */
  const reach = (names) => {
    const seen = new Map(); // name -> the function it was reached through ('' when direct)
    const queue = [...names].map((name) => [name, '']);
    while (queue.length) {
      const [name, via] = queue.shift();
      if (seen.has(name)) continue;
      seen.set(name, via);
      const body = functions.get(name);
      if (body) for (const inner of body) if (!seen.has(inner)) queue.push([inner, via || name]);
    }
    return seen;
  };
  const breaches = [];
  for (const statement of statements) {
    if (statement.defines) continue;
    const reached = reachCache.get(statement) ?? reach(statement.names);
    for (const [name, via] of reached) {
      const at = declaredAt.get(name);
      if (at !== undefined && at > statement.index) {
        breaches.push({ statement, name, via, declared: statements[at] });
      }
    }
  }
  return breaches;
}

test('the scanner finds top-level statements and skips strings, templates, regexes and blocks', () => {
  const source = [
    "import { a } from './a.js';",
    'const re = /[{(]/g;',
    'const t = `',
    'if (x) {`;',
    'function f() {',
    '  return later + g();',
    '}',
    "const s = { later: '{' };",
    'const g = () => other;',
    'f();',
    'let later = 2;',
    'if (later) {',
    'const inner = 3;',
    '}',
    'let other;',
  ].join('\n');
  assert.deepEqual(
    topLevelStatements(source).map((s) => s.line),
    [1, 2, 3, 5, 8, 9, 10, 11, 12, 15],
  );
  assert.ok(!topLevelStatements(source)[4].names.has('later'), 'an object key is not a name');
  const breaches = bootOrderBreaches(source).map((b) => `${b.statement.line}:${b.name}:${b.via}`);
  assert.deepEqual(breaches.sort(), ['10:later:f', '10:other:f']);
});

test('page scripts run no code that can reach a module-level const, let or class declared below it', () => {
  const scripts = pageScripts();
  assert.ok(scripts.includes('brand-centre.js') && scripts.includes('templates.js'), 'found the page scripts');
  const problems = [];
  for (const script of scripts) {
    const source = readFileSync(join(WEB, script), 'utf8');
    for (const { statement, name, via, declared } of bootOrderBreaches(source)) {
      problems.push(
        `web/${script}:${statement.line}: \`${statement.text}\` can reach \`${name}\`${via ? ` (through ${via})` : ''}, ` +
          `declared later at line ${declared.line}; move that declaration above line ${statement.line}`,
      );
    }
  }
  assert.deepEqual(problems, [], `\n${problems.join('\n')}`);
});
