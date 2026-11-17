-- A source item's repository identity is separate from the path displayed in ShareCraft. The
-- latter may carry a collision suffix and can therefore never identify the repository entry.
alter table source_items add column repository_path text;
update source_items set repository_path = path;
alter table source_items alter column repository_path set not null;

alter table source_items drop constraint source_items_pkey;
alter table source_items add primary key (library_id, repository_path);
alter table source_items add constraint source_items_display_path_key unique (library_id, path);
