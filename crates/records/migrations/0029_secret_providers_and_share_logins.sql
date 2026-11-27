-- Secrets may live at an outside provider (a cloud key vault adapter shared by several front
-- ends). Such a row holds no ciphertext: the id names the value at the provider. A forgotten
-- outside value is deleted at the provider by a sweep, and its row after it.
alter table secrets
  add column provider text not null default 'local' check (provider in ('local', 'external')),
  add column forgotten_at timestamptz,
  alter column key_id drop not null,
  alter column nonce drop not null,
  alter column ciphertext drop not null,
  add constraint secrets_local_has_ciphertext
    check (provider = 'external' or (key_id is not null and nonce is not null and ciphertext is not null));
create index secrets_forgotten on secrets (forgotten_at) where forgotten_at is not null;

-- One optional login per file server, used for every approved UNC root on that server. The
-- password lives in the secret store; this row names it. A user name is not a secret.
create table share_logins (
  server text primary key check (server = lower(server) and char_length(server) between 1 and 255),
  username text not null check (char_length(username) between 1 and 256),
  password_secret uuid not null references secrets(id),
  set_by uuid not null references users(id),
  set_at timestamptz not null default now()
);
