-- create extension ltree;

create table pages (
  name              varchar(100) primary key,
  url               varchar(200) not null,
  category          ltree not null default '',
  check_interval    interval not null default '1:50',
  cooldown          interval not null default '23:50',
  delete_regex      varchar(100);
  enabled           boolean not null default 't',

  last_checked      timestamp with time zone,
  last_modified     timestamp with time zone,
  last_error        text,
  item_id           uuid,

  http_etag         varchar(100),
  http_body_hash    bytea
);

