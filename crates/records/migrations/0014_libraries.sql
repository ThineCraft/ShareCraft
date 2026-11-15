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

-- Malware scanning: the result belongs to the content, so the same bytes are scanned once.
-- Content stored before this, or while no scanner is configured, is 'unscanned'. 'pending'
-- waits for the scanner, 'failed' is retried at `next_scan_at` (`scan_attempts` failures so
-- far), 'too_large' is over the scanner's limit, 'infected' names the threat in
-- `scan_detail`, and 'released' was let through by an administrator. `scan_hold` is set once
-- any upload names the content: pending content is available only while it is not, that is
-- only when it was only ever saved from an editor.
alter table blobs
  add column scan text not null default 'unscanned'
    check (scan in ('pending', 'clean', 'infected', 'failed', 'too_large', 'unscanned',
                    'released')),
  add column scan_detail text check (char_length(scan_detail) <= 200),
  add column scanned_at timestamptz,
  add column scan_attempts integer not null default 0 check (scan_attempts >= 0),
  add column next_scan_at timestamptz,
  add column scan_hold boolean not null default false;
create index blobs_scan_due on blobs (next_scan_at) where scan in ('pending', 'failed');
create index blobs_scan_held on blobs (scan) where scan in ('infected', 'failed', 'too_large');

-- The scanner, reached over ICAP: no scanner until an administrator names one.
insert into settings (key, value) values
  ('scanner_url', '""'),
  ('scanner_timeout_seconds', '120'),
  ('scanner_max_bytes', '2147483648'),
  ('scanner_unavailable', '"hold"'),
  ('scanner_too_large', '"hold"')
on conflict (key) do nothing;
