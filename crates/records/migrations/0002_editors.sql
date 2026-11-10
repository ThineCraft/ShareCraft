-- A document names the editor that opens it. Every document made before this was plain text.
alter table documents
  add column editor text not null default 'text'
  check (editor in ('text', 'gridcraft', 'wordcraft', 'pdfcraft', 'solidcraft'));
