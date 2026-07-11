# Homebrew formula for vag. Canonical copy lives in the tap
# (cluely/homebrew-vag → `brew install cluely/vag/vag`); this in-repo copy
# is kept in sync by the release process. New release checklist:
#   1. Tag vX.Y.Z (matching Cargo.toml), push the tag.
#   2. Update `url` + `sha256` here
#      (sha256 of the source tarball: `curl -L <url> | shasum -a 256`).
#   3. Copy this file into the tap repo as Formula/vag.rb.
class Vag < Formula
  desc "Keyboard-driven organizer that embeds your Claude Code & Codex sessions"
  homepage "https://github.com/cluely/vag"
  url "https://github.com/cluely/vag/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_RELEASE_TARBALL_SHA256"
  license "MIT"
  head "https://github.com/cluely/vag.git", branch: "main"

  depends_on "rust" => :build
  # The diff view renders through delta when present (syntax highlighting,
  # the user's own delta themes). vag falls back to its builtin renderer
  # without it, but the default experience should ship batteries-included.
  depends_on "git-delta"

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      vag drives the real agent CLIs — install at least one of:
        claude code:  https://code.claude.com
        codex:        https://developers.openai.com/codex
      Then check your setup with: vag doctor
    EOS
  end

  test do
    assert_match "vag #{version}", shell_output("#{bin}/vag --version")
    system "#{bin}/vag", "doctor"
  end
end
