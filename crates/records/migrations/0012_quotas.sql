-- Site quotas. `used` is the sum of the sizes of every version of every item in the site, live
-- and in its bins (purged items' versions are given back); `reserved` is what saves, uploads and
-- copies under way, and open upload sessions, have set aside; `quota` is the most the site may
-- hold, if it has a limit. Each row is locked FOR UPDATE to check and change it.
create table site_usage (
  site_id uuid primary key references nodes(id),
  used bigint not null default 0 check (used >= 0),
  reserved bigint not null default 0 check (reserved >= 0),
  quota bigint check (quota >= 0)
);
insert into site_usage (site_id) select id from nodes where kind = 'site';

-- The size of each recorded version, as Git describes it (inline old versions included).
-- Versions recorded before this are filled from Git by the rebuild.
alter table revisions add column size bigint check (size >= 0);

-- Flags asking the next start to rebuild every site's `used`: `upgrade`, set here once, and
-- `dirty`, set whenever a change of usage could not be written.
create table usage_flags (
  name text primary key check (name in ('upgrade', 'dirty')),
  set_at timestamptz not null default now()
);
insert into usage_flags (name) values ('upgrade');

-- What an upload session does when its name is taken where it goes, if chosen when it started.
alter table upload_sessions
  add column on_conflict text check (on_conflict in ('replace', 'keep_both', 'skip'));
