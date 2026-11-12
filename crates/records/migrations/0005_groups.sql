-- Groups of people, which can be given roles like a person. An organisation group has a stored
-- name, unique ignoring case. A site group stores its site and purpose instead: its name is
-- built from the site's current name when read, so it follows renames and never collides.
-- Every site, deleted or not, has three: owners, members and visitors.
create table groups (
  id uuid primary key,
  name text check (char_length(name) between 1 and 60),
  site_id uuid references nodes(id),
  purpose text check (purpose in ('owners', 'members', 'visitors')),
  created_by uuid references users(id),
  created_at timestamptz not null default now(),
  check ((name is not null and site_id is null and purpose is null)
         or (name is null and site_id is not null and purpose is not null))
);
create unique index groups_name on groups (lower(name)) where site_id is null;
create unique index groups_site on groups (site_id, purpose);

create table group_members (
  group_id uuid not null references groups(id),
  user_id uuid not null references users(id),
  added_by uuid references users(id),
  added_at timestamptz not null default now(),
  primary key (group_id, user_id)
);
create index group_members_user on group_members (user_id);

-- Every site gets its three groups, assigned Owner, Editor and Viewer on the site.
insert into groups (id, site_id, purpose)
select gen_random_uuid(), s.id, p.purpose
from nodes s cross join (values ('owners'), ('members'), ('visitors')) p(purpose)
where s.parent_id is null;

insert into assignments (node_id, principal_kind, principal_id, role_id)
select g.site_id, 'group', g.id,
       case g.purpose when 'owners' then 'owner' when 'members' then 'editor' else 'viewer' end
from groups g where g.site_id is not null;

-- Every person's assignment on a site becomes a membership of the matching group, with an event
-- marked as the upgrade's. Assignments below sites are unchanged.
with moved as (
  delete from assignments a using nodes s
  where a.node_id = s.id and s.parent_id is null
    and a.principal_kind = 'user' and a.role_id in ('owner', 'editor', 'viewer')
  returning a.node_id, a.principal_id, a.role_id, a.granted_by, a.granted_at
), added as (
  insert into group_members (group_id, user_id, added_by, added_at)
  select g.id, m.principal_id, m.granted_by, m.granted_at
  from moved m join groups g
    on g.site_id = m.node_id
   and g.purpose = case m.role_id when 'owner' then 'owners'
                                  when 'editor' then 'members' else 'visitors' end
  returning group_id, user_id
)
insert into events (kind, actor, subject, node_id, detail)
select 'group.member_added', null, a.user_id, g.site_id,
       jsonb_build_object('group', a.group_id, 'purpose', g.purpose, 'upgrade', true)
from added a join groups g on g.id = a.group_id;
