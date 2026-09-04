# Sites & tags

Two ways to organize hosts:

- Tags are free-form, many per host (`prod`, `db`, `web`). They are pure labels: filter with
  `tag:NAME`, repeatable and ANDed.
- A site is **one** per host (a data center, a project, a customer). Sites group the idle
  list and filter with `site:NAME`, and they can optionally carry shared SSH defaults that
  member hosts inherit.

## Site defaults & inheritance

A site may define a default **user**, **port**, **jump host(s)** (the site's bastion), and
**identity file(s)**. At connect time each member host is resolved against them:

- the site's value fills in **only where the host leaves that field unset**, so the host
  always wins;
- auth is never inherited and stays per-host;
- a bare site (name only) is pure grouping.

Inherited defaults show up everywhere the command does: connect, `Ctrl-y` yank,
`sshelf print-command`, transfers, forwards. A host that names an *undefined* site still
groups under that name and just inherits nothing.

The `user@host:port` you see is the resolved one, so a host with no user of its own is
listed under the site's user in the TUI, in `sshelf list`, and in shell completion, and
searching for that user finds it. What `hosts.toml` stores is unchanged, and so is
`sshelf list --json`: an inherited field stays `null` there, with the resolved values
visible in the generated `command`.

## In the list

Idle (empty search box): hosts group under `── site (n) ──` headers, with `(no site)` last.
While filtering: a flat list with a dim `·site·` column; `site:NAME` narrows to one site.

## Managing sites (`F3`)

`a` add · `e`/`Enter` edit · `d` delete · `Ctrl-s` save · `Esc` cancel. Each site's form is a
name plus the optional defaults. **Renaming** a site updates its member hosts; **deleting**
one clears its members' site, so nothing dangles. Assign a host's site in the
[add/edit form](hosts.md) (`←`/`→` over the defined sites + `(none)`).

## From the CLI

```sh
sshelf sites                                        # sites, member counts, their defaults
sshelf sites --json                                 # machine-readable
sshelf sites add prod-dc -u deploy -J bastion.prod  # define a site with shared defaults
sshelf add web1 -H 10.0.0.4 --site prod-dc          # add a host into it
sshelf list site:prod-dc                            # filter by site
```

Storage: `[[site]]` entries in `hosts.toml`. See [Data model & files](data-model.md).
