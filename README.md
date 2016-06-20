# Pagefeed

This program watches a set of web pages and presents any changes as an
RSS feed. It's as barebones as can be, but it does what I need it to
do.

It runs as a FastCGI service and requires a PostgreSQL database.

## Installation

First, you need to create a database. The default config expects it to
be named 'pagefeed'. Use schema.sql to initialize the database.

This repo includes a systemd service file to start the daemon, but you
can use spawn-fcgi or even inetd. The daemon accepts one optional
command line argument, a [rust-postgres connection URL][connect].

[connect]: https://sfackler.github.io/rust-postgres/doc/v0.11.7/postgres/struct.Connection.html#method.connect

Finally, you need to configure your web server. E.g. if you use nginx:

    location /pagefeed {
      include fastcgi.conf;
      fastcgi_param SCRIPT_NAME /pagefeed;
      fastcgi_pass unix:/run/pagefeed.socket;

      allow 127.0.0.1;
      deny all;
    }

## Adding pages to be watched

Sorry, this thing has no user interface.

    insert into pages (name, url) values ('Zombo.com', 'http://zombo.com/');
