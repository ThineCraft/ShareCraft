-- How a library's hero image is framed in the header (mirrored from `hero_frame` in its record
-- in Git): {x, y, zoom}, the point of the picture kept in view in thousandths across and down
-- (0 to 1000) and the enlargement in percent (100 to 400). Null for the middle at its smallest.
alter table libraries add column hero_frame jsonb;
