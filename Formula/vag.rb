# Homebrew formula for vag.
#
# NOT PUBLISHED YET. When the repository goes public:
#   1. Replace OWNER below with the GitHub org/user.
#   2. Tag a release (vX.Y.Z), update `url` + `sha256`
#      (sha256 of the source tarball: `curl -L <url> | shasum -a 256`).
#   3. Push this file to a tap repo (homebrew-vag) as Formula/vag.rb;
#      users then: `brew install OWNER/vag/vag`.
class Vag < Formula
  desc "Keyboard-driven organizer that embeds your Claude Code & Codex sessions"
  homepage "https://github.com/OWNER/vag"
  url "https://github.com/OWNER/vag/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_RELEASE_TARBALL_SHA256"
  license "MIT"
  head "https://github.com/OWNER/vag.git", branch: "main"

  depends_on "rust" => :build

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
