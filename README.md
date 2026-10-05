# remote-plugin-sicompass

*Browse lists your own servers serve, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

Remote shows lists that servers of your choice serve over HTTP, in the same
keyboard-navigable tree as everything else in Sicompass. Each server you add
appears as its own section. A server answers `GET <url>/root` with a JSON list,
and `GET <url>/<entry>` with the contents of one of its entries.

Add servers in Settings, under remote, one per line as `name URL`. If a server
needs a key, add it under API keys as `name key`, and Remote sends it as a
bearer token.

Because the servers are yours to choose, Remote asks for access to any server
on the internet. The Store shows that before you install it, and installing it
is your approval. It connects only to the servers you add.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
remote. The Store checks the release's signature before installing it, and
keeps it up to date.

To install a build of your own instead, copy `plugin.json`, the built
`plugin` program (`plugin.exe` on Windows) and `locales/` into a folder named
`remote` in the Sicompass plugins folder (`~/.config/sicompass/plugins/` on
Linux, `~/Library/Application Support/sicompass/plugins/` on macOS) and restart
Sicompass.

## Building from source

```bash
nix develop          # the toolchain
cargo test           # the tree logic and the HTTP client
cargo build --release
cp target/release/remote-plugin plugin
```

`./scripts/release-plugin.sh --dry-run` builds this computer's release, packs
it, and signs and verifies it with a throwaway key, the way a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK and the plugin kit

## Community

Join the conversation on
[Discord](https://discord.com/channels/1464152138753249313/1464152139231137894).

## License

#### Open source license

If you are creating an open source application under a license compatible with
the GNU GPL license v3, you may use this project under the terms of the GPLv3.
See [LICENSE](LICENSE).

## Contributing

Contributions are welcome. Whether it is code, documentation, or feedback, your
input helps make computing more accessible for everyone.
