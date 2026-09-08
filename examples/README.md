# Examples

Copy one into a project as `.agentbox.toml` and edit it:

```console
$ cp ~/dev/agentbox/examples/rust-workspace.agentbox.toml ~/dev/api/.agentbox.toml
```

| File | Shape of the project |
| --- | --- |
| `rust-workspace.agentbox.toml` | Rust service, sibling crate as a path dependency, read-only reference implementation, shared crate cache |
| `node-monorepo.agentbox.toml` | pnpm monorepo, browser tests, `node_modules` kept inside the box |
| `offline-review.agentbox.toml` | `network = "none"`, tools baked in at creation, for review and refactor tasks |

Replace `USER` in any path with your username, or use `~`, which expands to
your home on the host side and the sandbox user's home inside the box — the
same string, since the usernames match.
