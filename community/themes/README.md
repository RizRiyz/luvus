# Luvus community themes

This directory is the reviewed, data-only theme registry used by:

```sh
luvus theme install community/<theme-id>
```

Each theme is one schema-1 TOML file named `<theme-id>.toml`. Theme IDs are
lowercase and may contain letters, numbers, dots, dashes, or underscores. A
submission must not shadow a bundled theme or the virtual `terminal` theme.

## Submit a theme

1. Create and preview the palette at <https://luvus.dev/themes/>.
2. Use **Publish theme**, or add `community/themes/<theme-id>.toml` in a fork.
3. Keep the pull request limited to one theme unless the files are an intentional
   parent/child family.
4. Run:

   ```sh
   cargo run -- theme validate community/themes/<theme-id>.toml --strict
   cargo test --locked community_theme_files_are_valid
   ```

Theme files may contain metadata and the 18 documented semantic colors only.
They cannot contain scripts, CSS, keybindings, terminal commands, or other
executable behavior. Reviewed files require Luvus 0.12 or newer and use this
repository's license, so contributors do not add a per-theme `license` field.
Complete themes are preferred for portability. A partial
theme must declare one installed `extends` parent; inheritance is limited to
eight levels and cycles are rejected.

Community coordinates are fetched over HTTPS from the repository's `main`
branch. Every pull request is reviewed and CI validates the complete registry
before merge.
