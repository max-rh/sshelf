# Sites & tags

Two ways to organize hosts:

- **Tags** — free-form, many per host (`prod`, `db`, `web`). Pure labels: filter with
  `tag:NAME`, repeatable and ANDed.
- **A site** — **one** per host (a data center, a project, a customer). Sites group the idle
  list, filter with `site:NAME` — and can optionally carry **shared SSH defaults** that
  member hosts inherit.

## Site defaults & inheritance

A site may define a default **user**, **port**, **jump host(s)** (the site's bastion), and
**identity file(s)**. At connect time each member host is resolved against them:

- the site's value fills in **only where the host leaves that field unset** — the host
  always wins;
- **auth is never inherited** — it stays per-host;
- a bare site (name only) is pure grouping.

Inherited defaults show up everywhere the command does: connect, `Ctrl-y` yank,
`sshelf print-command`, transfers, forwards. A host that names an *undefined* site still
groups under that name — it just inherits nothing.

## In the list

Idle (empty search box): hosts group under `── site (n) ──` headers, with `(no site)` last.
While filtering: a flat list with a dim `·site·` column; `site:NAME` narrows to one site.

## Managing sites (`F3`)

`a` add · `e`/`Enter` edit · `d` delete · `Ctrl-s` save · `Esc` cancel. Each site's form is a
name plus the optional defaults. **Renaming** a site updates its member hosts; **deleting**
one clears its members' site — nothing dangles. Assign a host's site in the
[add/edit form](hosts.md) (`←`/`→` over the defined sites + `(none)`).

## From the CLI

```sh
sshelf sites                                        # sites, member counts, their defaults
sshelf sites --json                                 # machine-readable
sshelf sites add prod-dc -u deploy -J bastion.prod  # define a site with shared defaults
sshelf add web1 -H 10.0.0.4 --site prod-dc          # add a host into it
sshelf list site:prod-dc                            # filter by site
```

Storage: `[[site]]` entries in `hosts.toml` — see [Data model & files](data-model.md).
