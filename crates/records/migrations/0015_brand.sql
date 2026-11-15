-- Branding. The organisation's brand centre (themes, brand images and brand managers) is
-- mirrored from its record in Git, org/brand.json, as one row, which always exists: it starts
-- as an empty centre, as Git reads with no record.
create table org_brand (
  id boolean primary key check (id),
  record jsonb not null
);
insert into org_brand (id, record)
values (true, '{"schema": 1, "managers": [], "themes": [], "images": []}');

-- The brand images in use, which pin their content in the content store: one row per content
-- and owner, `org` for the brand centre or a site's id for its look. Erasure counts these rows
-- as it counts `blob_refs`, so pinned content is never erased. A row is written in the same
-- transaction as the mirror change that names the image, after the content's `blobs` row is
-- locked FOR SHARE. `media_type` is the type judged from the content when it was chosen, the
-- one it is served as. Git stays the authority: the rows are rebuilt from it.
create table brand_refs (
  sha256 text not null references blobs(sha256),
  owner text not null
    check (owner = 'org'
           or owner ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
  media_type text not null
    check (media_type in ('image/png', 'image/jpeg', 'image/webp', 'image/gif',
                          'image/svg+xml')),
  primary key (owner, sha256)
);
create index brand_refs_sha256 on brand_refs (sha256);

-- A site's look and its own themes, mirrored from its record in Git (sites/<xx>/<id>.json,
-- `look` and `themes`). A site with no look reads as the built-in ShareCraft theme with the
-- defaults, as every site did before branding.
alter table sites
  add column look jsonb null,
  add column themes jsonb not null default '[]';
