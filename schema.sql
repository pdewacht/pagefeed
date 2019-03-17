create table pages (
  slug              varchar(100) primary key,
  name              varchar(100) not null,
  url               varchar(200) not null,
  enabled           boolean not null default 't',
  delete_regex      varchar(200),
  check_interval    interval not null default '1:50',
  cooldown          interval not null default '23:50',

  last_checked      timestamp with time zone,
  last_modified     timestamp with time zone,
  last_error        text,
  item_id           uuid,

  http_etag         varchar(100),
  http_body_hash    bytea
);

