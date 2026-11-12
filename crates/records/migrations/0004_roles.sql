-- Roles are named sets of permissions, and assignments give a principal (a person, or from the
-- next migration a group) a role on a scope. perms holds one bit per permission of the fixed
-- list: 1 view, 2 add, 4 edit, 8 delete, 16 manage_access, 32 manage_site. Every role has view.
create table roles (
  id text primary key,
  name text not null check (char_length(name) between 1 and 60),
  perms integer not null check (perms between 1 and 63 and perms & 1 = 1),
  builtin boolean not null default false,
  created_by uuid references users(id),
  created_at timestamptz not null default now()
);
create unique index roles_name on roles (lower(name));

-- The built-in roles: fixed ids, never changed or deleted.
insert into roles (id, name, perms, builtin) values
  ('viewer', 'Viewer', 1, true),
  ('editor', 'Editor', 15, true),
  ('owner', 'Owner', 63, true);

create table assignments (
  node_id uuid not null references nodes(id),
  principal_kind text not null check (principal_kind in ('user', 'group')),
  principal_id uuid not null,
  role_id text not null references roles(id),
  granted_by uuid references users(id),
  granted_at timestamptz not null default now(),
  primary key (node_id, principal_kind, principal_id)
);
create index assignments_principal on assignments (principal_kind, principal_id);
create index assignments_role on assignments (role_id);

-- Every plan-1 grant becomes a person's assignment with the role of the same name. The old table
-- is kept, unread, so that an upgrade can be inspected and undone; a later migration drops it.
insert into assignments (node_id, principal_kind, principal_id, role_id, granted_by, granted_at)
select node_id, 'user', user_id, role, granted_by, granted_at from scope_access;
alter table scope_access rename to scope_access_plan1;
