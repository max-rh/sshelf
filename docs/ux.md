# UI design notes

How the interface is designed and why. **What each screen does — and every keybinding — is
documented in the user Guide** ([Searching & connecting](search-connect.md),
[Adding & editing hosts](hosts.md), [Transferring files](transfer.md),
[Port forwarding](port-forwarding.md), [Sites & tags](sites-tags.md)); this page holds the
design rationale and rendering details behind those screens.

## Visual model

atuin.sh: slim chrome, an inline filter-as-you-type list, and a contextual keybind hint bar
at the bottom. The search box is **always active** (single-mode, no insert/normal split), so
plain letters filter the list; actions therefore use **Ctrl** or function keys, which can't
be typed into the query.

Main screen layout: `Length(3)` search · `Min(0)` list · `Length(1)` hint bar. Each row shows
`name · user@host[:port] · [tags]`, plus a dim `·site·` column while filtering. The
`matched/total` count lives in the search-box *title* so it's never truncated by a narrow
terminal.

## Sorting / ranking

- **No query (idle):** frecency desc —
  `score = use_count * exp(-decay_rate * days_since_last_used)`, `decay_rate` default `0.2`
  (`default_sort = "name"` opts out). The idle view groups by site.
- **Typing:** fuzzy-filter via `nucleo-matcher`; sort by match score with frecency breaking
  ties. Matched characters are highlighted (bold/accent) using the matcher's match indices,
  rendered with `unicode-width` so wide/combining characters don't misalign.
- Fuzzy only for now (prefix/substring modes can come later).

## The add/edit form

A single-screen, **auth-aware** field form rather than a paged wizard — simpler to navigate
and edit, "guided" by dim placeholders (`required ·` / `optional ·`) and inline validation
with focus jumping to the offending field. Fields specific to an auth method only render for
that method. The Key field is a picker (single key) backed by a fuzzy file-browser modal;
key discovery matches keypairs (`.pub` sibling) *and* standalone private keys by their
`PRIVATE KEY` header so `.pem` files show up. A host configured with multiple identity files
keeps them on edit; entering several is done by editing `hosts.toml`.

## Modality & precedence

Full-screen modes (transfer, sites, forwards manager) and popups (forward, 2FA, confirms)
route keys **before** the list screen — first match wins — and render in the same precedence
order, so input routing and drawing can't disagree. Destructive actions (delete a host, stop
a forward) confirm with `y`; any other key cancels. Help (`F1`) is an overlay listing every
key; any key closes it.

## Theming

atuin-inspired defaults: dim chrome and a single accent color (config key `accent`) for the
selection + match highlights. Terminal resize is handled by ratatui's layout pass — no manual
recompute.
