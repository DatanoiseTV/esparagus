# typed: false
# frozen_string_literal: true

# Homebrew formula for esparagus.
#
# Place this file in a Homebrew tap repository named
# `homebrew-esparagus` (or rename to `homebrew-tap` and adjust the
# tap commands below).  Once published, users install with:
#
#     brew tap DatanoiseTV/esparagus
#     brew install esparagus
#
# This formula builds from source via Rust/Cargo. For a binary-only
# install (no Rust toolchain required), publish the prebuilt
# artefacts from `.github/workflows/release.yml` and switch to a
# `bottle` block — see the Homebrew docs.
class Esparagus < Formula
  desc "ESP32 family flasher with structured observability for CI/CD and LLM feedback loops"
  homepage "https://github.com/DatanoiseTV/esparagus"
  license "GPL-2.0-or-later"
  head "https://github.com/DatanoiseTV/esparagus.git", branch: "master"

  # Update `url` and `sha256` on each release.  The sha256 lines from
  # the release pipeline's `*.sha256` artefacts are the source of
  # truth — copy the source-tarball line directly.
  #   shasum -a 256 v0.1.0.tar.gz
  url "https://github.com/DatanoiseTV/esparagus/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  version "0.1.0"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    # Smoke test: the binary's own --version + a known-output
    # subcommand. `list-ports` is safe to run anywhere (it doesn't
    # touch a chip).
    assert_match version.to_s, shell_output("#{bin}/esparagus --version")
    system bin/"esparagus", "list-ports"
  end
end
