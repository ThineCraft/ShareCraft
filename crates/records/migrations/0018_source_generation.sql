-- A connection's generation: moved on by every change of its configuration and by "Sync now".
-- A sync records its result only if the generation is still the one it started with, so a
-- sync that read the connection before a change never writes its bookmark over that change.
alter table library_sources add column generation bigint not null default 0;
