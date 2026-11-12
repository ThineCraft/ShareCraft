-- The live scopes (nodes with their own assignments): the inheritance explorer starts from these
-- and walks up from each, so its cost follows the number of breaks, never the number of files.
create index nodes_scopes on nodes (id) where unique_scope and deleted_at is null;

-- A folder's live children in the order the listing shows them: folders (and libraries) before
-- files, then by name ignoring case, then by id. The expression is the listing's own, so a page
-- of a folder of 100,000 files is a range scan from its cursor.
create index nodes_children
  on nodes (parent_id, (case when kind = 'document' then 1 else 0 end), lower(name), id)
  where deleted_at is null;
