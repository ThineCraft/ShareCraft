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

-- A file no editor opens is a document of the editor 'other': any file type.
alter table documents drop constraint documents_editor_check;
alter table documents add constraint documents_editor_check
  check (editor in ('text', 'gridcraft', 'wordcraft', 'pdfcraft', 'solidcraft', 'other'));

-- The organisation's block list of file types: extensions refused in every library. None by
-- default.
insert into settings (key, value) values ('blocked_file_types', '[]')
on conflict (key) do nothing;
