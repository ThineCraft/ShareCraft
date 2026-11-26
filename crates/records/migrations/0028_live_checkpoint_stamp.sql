-- A checkpoint loads under the server's stamp on the entry at its position: when that entry was
-- put in order and its seed. The 30-day sweep deletes old entries while a verified checkpoint
-- keeps its identity data, so the checkpoint keeps its own copy of the stamp. Both or neither:
-- a checkpoint at position 0, or one whose entry was already gone, has none.
alter table live_checkpoints
  add column entry_at timestamptz,
  add column entry_seed bigint check (entry_seed between 0 and 4294967295),
  add constraint live_checkpoints_entry_stamp_check
    check ((entry_at is null) = (entry_seed is null));

update live_checkpoints c
   set entry_at = e.at, entry_seed = e.seed
  from live_entries e
 where e.document_id = c.document_id and e.seq = c.through;
