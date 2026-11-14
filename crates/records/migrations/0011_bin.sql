-- The two-stage recycle bin, mirrored from Git like the rest of the tree. A deleted node names
-- the item whose deletion took it to the bin (`deleted_with`: its own id for the item a person
-- deleted, that item's id for everything deleted with it). The item a person deleted, the top
-- of its deletion, is the bin's entry and records its stage (1 or 2). A purged node keeps its
-- row as a tombstone (`purged_at`), never listed and never reachable.
alter table nodes
  add column deleted_with uuid,
  add column bin_stage smallint check (bin_stage in (1, 2)),
  add column purged_at timestamptz;

-- Nodes deleted before the bin were each deleted on their own: each is its own entry, in the
-- first stage.
update nodes set deleted_with = id, bin_stage = 1 where deleted_at is not null;

alter table nodes
  add constraint nodes_bin_state check (
    (deleted_at is null) = (deleted_with is null)
    and (bin_stage is null or deleted_at is not null)
    and (purged_at is null or deleted_at is not null));

-- The entries of a site's bin, newest first, and the entries due to be purged.
create index nodes_bin on nodes (site_id, bin_stage, deleted_at desc, id)
  where deleted_with = id and purged_at is null;
create index nodes_purge_due on nodes (deleted_at)
  where deleted_with = id and purged_at is null;
-- What was deleted with an entry.
create index nodes_deleted_with on nodes (deleted_with) where deleted_with is not null;
