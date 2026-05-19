# Homebrew formula

`esparagus.rb` is the Homebrew formula for esparagus. To publish:

1. Create a new GitHub repo `DatanoiseTV/homebrew-esparagus` (the
   `homebrew-` prefix is what makes it a valid tap).
2. Drop `esparagus.rb` at the repo root.
3. Bump `url`, `version`, and `sha256` on each release. Get the
   `sha256` of the source tarball with:

   ```sh
   curl -L https://github.com/DatanoiseTV/esparagus/archive/refs/tags/vX.Y.Z.tar.gz \
     | shasum -a 256
   ```

   or copy it from the `.sha256` asset produced by the release
   workflow.

4. Users then install with:

   ```sh
   brew tap DatanoiseTV/esparagus
   brew install esparagus
   ```

The formula builds from source via `cargo install`. For a faster
binary-bottle install, follow [Homebrew's bottle
docs](https://docs.brew.sh/Bottles) and reference the
`x86_64-apple-darwin` / `aarch64-apple-darwin` tarballs the release
workflow uploads.
