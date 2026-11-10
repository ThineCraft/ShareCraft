create table users (
  id uuid primary key,
  email text not null,
  name text not null,
  password_hash text not null,
  is_admin boolean not null default false,
  disabled_at timestamptz,
  created_at timestamptz not null default now()
);
create unique index users_email on users (lower(email));

create table sessions (
  id uuid primary key,
  token_hash bytea not null unique,
  user_id uuid not null references users(id),
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  ended_at timestamptz
);

create table documents (
  id uuid primary key,
  name text not null,
  created_by uuid references users(id),
  created_at timestamptz not null default now(),
  revision text,                       -- current Git revision; null until first saved
  revised_at timestamptz,
  fields jsonb not null default '{}'
);

create table document_access (
  document_id uuid not null references documents(id),
  user_id uuid not null references users(id),
  role text not null check (role in ('viewer','editor','owner')),
  granted_by uuid references users(id),
  granted_at timestamptz not null default now(),
  primary key (document_id, user_id)
);
create index document_access_user on document_access (user_id);

create table revisions (
  revision text primary key,           -- Git commit id
  document_id uuid not null references documents(id),
  position integer not null,           -- 1 for the document's first revision
  parent text,                         -- the document's previous revision
  committed_at timestamptz not null,
  authors jsonb not null,              -- [{"id": "...", "name": "..."}], commit author first
  unique (document_id, position)
);

create table events (
  id bigint generated always as identity primary key,
  at timestamptz not null default now(),
  kind text not null,
  actor uuid,                          -- who did it; null for the server itself
  subject uuid,                        -- the account acted on, if any
  document_id uuid,
  detail jsonb not null default '{}'
);
create index events_document on events (document_id, id);
