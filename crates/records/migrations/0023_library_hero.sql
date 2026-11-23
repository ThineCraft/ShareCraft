-- A library's hero image (mirrored from `hero_image` in its record in Git): shown in place of the
-- site header's banner image on the library's pages. The image as chosen, an ImageRef as JSON
-- ({sha256, media_type, bytes, width?, height?, name, from_doc}); its content is pinned in
-- brand_refs with the library's id as owner, as a site's look pins its images.
alter table libraries add column hero_image jsonb;
