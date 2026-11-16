-- Brand fonts (plan 4B). Font files are pinned in brand_refs exactly as brand images are, so
-- the types a pin may be served as now include the four font types. A site's own font
-- packages are mirrored from its record in Git (sites/<xx>/<id>.json, `font_packages`); a site
-- with none has an empty list, as every site had before.
alter table brand_refs drop constraint brand_refs_media_type_check;
alter table brand_refs add constraint brand_refs_media_type_check
  check (media_type in ('image/png', 'image/jpeg', 'image/webp', 'image/gif',
                        'image/svg+xml', 'font/woff2', 'font/woff', 'font/ttf', 'font/otf'));

alter table sites add column font_packages jsonb not null default '[]';
