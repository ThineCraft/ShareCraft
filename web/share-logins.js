// The Logins part of the File shares settings: the pure helpers (which servers can have a
// login, the rows, the sentences). The page itself is in sharing.js. A password never passes
// through here: the server never answers one.

/** The lower-case host of a UNC root (\\host\share\... gives host), or null for any other root. */
export function serverOfRoot(root) {
  const match = /^\\\\([^\\/]+)(?:[\\/]|$)/.exec(String(root ?? ''));
  return match ? match[1].toLowerCase() : null;
}

/** The servers of the approved UNC roots, each once, in the order they first appear. */
export function loginServers(roots) {
  const servers = [];
  for (const root of Array.isArray(roots) ? roots : []) {
    const server = serverOfRoot(root);
    if (server && !servers.includes(server)) servers.push(server);
  }
  return servers;
}

/** The rows of the Logins table; `when` turns a set_at time into text. Nothing else is copied. */
export function loginRows(logins, when = (iso) => String(iso ?? '')) {
  return (Array.isArray(logins) ? logins : []).map((login) => ({
    server: login.server,
    username: login.username,
    set: when(login.set_at),
    unc: `\\\\${login.server}`,
  }));
}

export const LOGIN_HELP =
  'ShareCraft signs in to this server with this login, for every approved folder on it. The password is stored in the secret store and never shown again.';

export const runsAsLoginLine = (runsAs) => `Without a login, ShareCraft reads as ${runsAs}.`;

export const removeLoginQuestion = (server, runsAs) =>
  `Remove the login for \\\\${server}? Folders on it will be read as ${runsAs}.`;

/** The sentence for a refused login, or '' for a code that is not about logins. */
export function loginErrorText(code, server = '') {
  const host = server ? `\\\\${server}` : 'the server';
  switch (code) {
    case 'login_refused':
      return `${host} refused this user name and password. Check them and try again.`;
    case 'login_conflict':
      return `${host} is already connected with other credentials, for example a drive mapped by the account ShareCraft runs under. Disconnect that connection on the ShareCraft server, then save the login again.`;
    case 'server_unreachable':
      return `ShareCraft could not reach ${host}. Check the name and that the server is on.`;
    case 'no_root_on_server':
      return `No approved folder is on ${host}. Approve a folder there first.`;
    case 'logins_windows_only':
      return 'Share logins work only when ShareCraft runs on Windows.';
    case 'secret_store_unavailable':
      return 'The secret store could not be reached. Nothing was saved. Try again in a moment.';
    case 'invalid_request':
      return 'Enter a user name of 1 to 256 characters without a /, and a password of 1 to 256 characters.';
    default:
      return '';
  }
}
