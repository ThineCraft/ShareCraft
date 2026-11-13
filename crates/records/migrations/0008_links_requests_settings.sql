-- Organisation settings, one row per known key. Anyone links are off until an administrator
-- switches them on, and expire within 30 days at most.
create table settings (
  key text primary key,
  value jsonb not null,
  changed_by uuid references users(id),
  changed_at timestamptz not null default now()
);
insert into settings (key, value) values
  ('anyone_links', 'false'),
  ('anyone_link_max_days', '30');

-- A signed-in link is a hidden group (purpose 'link', no name and no site) with an assignment
-- on the shared item. The unique index on (site_id, purpose) stays whole: a null site never
-- collides, and making a site's groups relies on it.
alter table groups drop constraint groups_purpose_check;
alter table groups add constraint groups_purpose_check
  check (purpose in ('owners', 'members', 'visitors', 'link'));
alter table groups drop constraint groups_check;
alter table groups add constraint groups_check
  check ((name is not null and site_id is null and purpose is null)
         or (name is null and site_id is not null and purpose in ('owners', 'members', 'visitors'))
         or (name is null and site_id is null and purpose = 'link'));

-- Sharing links. Only a SHA-256 hash of a link's token is stored. A signed-in link has its
-- group; an anyone link has none, is always Viewer and always expires. A revoked link's row is
-- kept, with who revoked it (null for the server) and why; its role becomes null if that role
-- is deleted later.
create table links (
  id uuid primary key,
  node_id uuid not null references nodes(id),
  kind text not null check (kind in ('signed_in', 'anyone')),
  role_id text references roles(id) on delete set null,
  token_hash bytea not null unique,
  group_id uuid references groups(id),
  created_by uuid references users(id),
  created_at timestamptz not null default now(),
  expires_at timestamptz,
  revoked_at timestamptz,
  revoked_by uuid references users(id),
  revoke_reason text,
  check ((kind = 'signed_in') = (group_id is not null)),
  check (kind = 'signed_in' or (role_id = 'viewer' and expires_at is not null)),
  check (role_id is not null or revoked_at is not null),
  check ((revoked_at is null) = (revoke_reason is null))
);
create unique index links_group on links (group_id);
create index links_node on links (node_id) where revoked_at is null;
create index links_expiry on links (expires_at) where revoked_at is null;

-- Requests for access to an item. One open request per person per item; deciding one records
-- the decision ('closed' when the item was deleted before anyone decided).
create table access_requests (
  id uuid primary key,
  node_id uuid not null references nodes(id),
  requester uuid not null references users(id),
  message text check (char_length(message) <= 500),
  created_at timestamptz not null default now(),
  decided_at timestamptz,
  decided_by uuid references users(id),
  decision text check (decision in ('approved', 'declined', 'closed')),
  role_id text references roles(id) on delete set null,
  reply text check (char_length(reply) <= 500),
  check ((decided_at is null) = (decision is null))
);
create unique index access_requests_open on access_requests (node_id, requester)
  where decided_at is null;
create index access_requests_requester on access_requests (requester, decided_at);
create index access_requests_node_open on access_requests (node_id) where decided_at is null;

-- Every submission of a request, recorded or not, for the daily limit per person.
create table request_attempts (
  requester uuid not null references users(id),
  at timestamptz not null default now()
);
create index request_attempts_requester on request_attempts (requester, at);
