-- Sites' own settings, mirrored from their records in Git (sites/<xx>/<id>.json) as the tree
-- is. A site created before site records has no record: it reads as a team site with no
-- description and the organisation's default language and time zone, as filled here.
create table sites (
  id uuid primary key references nodes(id),
  kind text not null default 'team' check (kind in ('team', 'communication')),
  description text not null default '' check (char_length(description) <= 500),
  language text not null check (char_length(language) between 2 and 35),
  time_zone text not null check (char_length(time_zone) between 1 and 100),
  template text,
  archived_at timestamptz,
  archived_by uuid references users(id),
  is_hub boolean not null default false,
  hub_navigation jsonb,
  joined_hub uuid references sites(id),
  navigation jsonb,
  -- A purge of the deleted site has started and is finished by the sweeper after a restart.
  purging boolean not null default false,
  check (is_hub or hub_navigation is null),
  check (archived_by is null or archived_at is not null)
);
create index sites_joined_hub on sites (joined_hub) where joined_hub is not null;

-- The organisation's regional defaults, and the mirror of the global navigation
-- (org/navigation.json), which is changed only through its own route.
insert into settings (key, value) values
  ('default_language', '"en"'),
  ('default_time_zone', '"UTC"'),
  ('global_navigation', '[]')
on conflict (key) do nothing;

insert into sites (id, language, time_zone)
select id, 'en', 'UTC' from nodes where kind = 'site';

-- Templates saved from a site or imported, mirrored from templates/<xx>/<id>.json.
create table site_templates (
  id uuid primary key,
  name text not null check (char_length(name) between 1 and 80),
  description text not null default '' check (char_length(description) <= 500),
  body jsonb not null,
  created_by uuid references users(id),
  created_at timestamptz not null,
  deleted_at timestamptz
);
