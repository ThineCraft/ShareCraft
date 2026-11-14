-- Libraries' kinds and settings, mirrored from their records in Git (libraries/<xx>/<id>.json)
-- as site records are. A library made before library records has no record: it reads as a
-- document library with no description, folders on, every type allowed, nothing blocked and no
-- limit, as filled here.
create table libraries (
  id uuid primary key references nodes(id),
  kind text not null default 'document'
    check (kind in ('document', 'image', 'template', 'asset')),
  description text not null default '' check (char_length(description) <= 500),
  folders boolean not null default true,
  -- Extensions, lower case with their leading dot; null for every type.
  allowed_types text[] check (allowed_types is null or cardinality(allowed_types) <= 100),
  blocked_types text[] not null default '{}' check (cardinality(blocked_types) <= 100),
  max_file_bytes bigint check (max_file_bytes is null or max_file_bytes > 0)
);
create index libraries_kind on libraries (kind) where kind <> 'document';

insert into libraries (id)
select id from nodes where kind = 'library';
