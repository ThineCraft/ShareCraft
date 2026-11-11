-- The content tree: sites, libraries, folders and files. A file's node id is its document id.
-- Git holds the authority for the tree (tree/<id>.json); this table mirrors it. Permissions
-- live only here: a node either inherits or is a scope with its own rows in scope_access.
create table nodes (
  id uuid primary key,
  kind text not null check (kind in ('site', 'library', 'folder', 'document')),
  parent_id uuid references nodes(id),
  site_id uuid not null references nodes(id) deferrable initially deferred,
  name text not null,
  unique_scope boolean not null default false,
  deleted_at timestamptz,
  deleted_by uuid,
  created_by uuid,
  created_at timestamptz not null,
  check ((kind = 'site') = (parent_id is null))
);
create unique index nodes_sibling_name on nodes (parent_id, lower(name)) where deleted_at is null;
create index nodes_parent on nodes (parent_id);

create table scope_access (
  node_id uuid not null references nodes(id),
  user_id uuid not null references users(id),
  role text not null check (role in ('viewer','editor','owner')),
  granted_by uuid references users(id),
  granted_at timestamptz not null default now(),
  primary key (node_id, user_id)
);
create index scope_access_user on scope_access (user_id);

alter table events add column node_id uuid;
create index events_node on events (node_id, id);
