-- Migration 0005 made every site's three groups without recording them. Each site group with no
-- group.created event gets one now, marked as the upgrade's, with the detail the server records
-- when it makes a site's groups.
insert into events (kind, actor, subject, node_id, detail)
select 'group.created', null, null, g.site_id,
       jsonb_build_object('id', g.id, 'site', g.site_id, 'purpose', g.purpose, 'upgrade', true)
from groups g
where g.site_id is not null
  and not exists (select 1 from events e
                  where e.kind = 'group.created' and e.detail->>'id' = g.id::text)
order by g.site_id, g.id;

-- A site's live scopes: the inheritance explorer's walk starts from those of one site, and reads
-- no other site's.
create index nodes_site_scopes on nodes (site_id) where unique_scope and deleted_at is null;
