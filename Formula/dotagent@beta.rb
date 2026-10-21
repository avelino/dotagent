# dotagent@beta — Homebrew formula for the rolling beta channel.
#
# This formula tracks the `main` branch. Every push to main triggers
# `.github/workflows/release-beta.yml`, which:
#
#   1. Builds dotagent for the 4 release targets.
#   2. Deletes the previous `beta` GitHub release + tag and republishes
#      a fresh `beta` release (`prerelease: true`) at the new commit.
#      The tag is rolling — only the most recent main build is reachable.
#   3. Rewrites `version` + the four `sha256` lines below using the
#      `# DOTAGENT_BETA_*` markers, then commits the change to main.
#
# Install:
#
#     brew tap avelino/dotagent https://github.com/avelino/dotagent
#     brew install dotagent@beta
#     brew link --overwrite dotagent@beta    # puts `dotagent` on $PATH
#
# `@beta` is `keg_only` per Homebrew convention for versioned formulae,
# so it doesn't auto-link. `brew link --overwrite` replaces any stable
# `dotagent` on $PATH with the beta binary; reverse with
# `brew unlink dotagent@beta && brew link dotagent`.
#
# The `version` field uses `0.0.1-beta.<commit-count-on-main>`, which
# is monotonic so `brew upgrade` correctly detects new builds.

class DotagentATBeta < Formula
  desc "Polyglot agent orchestrator (beta — built from main)"
  homepage "https://github.com/avelino/dotagent"
  version "0.6.2-beta.85" # DOTAGENT_BETA_VERSION
  license "MIT"

  keg_only :versioned_formula

  on_macos do
    on_arm do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-aarch64-darwin.tar.gz"
      sha256 "8c911b532ca814a866ef1feb832b5db94ceb6583b3c23def78b71e73ba82117d" # DOTAGENT_BETA_AARCH64_DARWIN_SHA
    end
    on_intel do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-x86_64-darwin.tar.gz"
      sha256 "e8e6ac89de7654fc884d3df0683b2e57b908692cddd4f5991dd3b10e9022a8cb" # DOTAGENT_BETA_X86_64_DARWIN_SHA
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-aarch64-linux.tar.gz"
      sha256 "0e455c58998c6dbeaa2b4a2453175ea47b150493efb71b15dcb68a156d83dd8f" # DOTAGENT_BETA_AARCH64_LINUX_SHA
    end
    on_intel do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-x86_64-linux.tar.gz"
      sha256 "0964d34a464185bf71a89847fe90fb9c7d8f6924856831daf8e58a827b02cc46" # DOTAGENT_BETA_X86_64_LINUX_SHA
    end
  end

  def install
    bin.install Dir["bin/*"]
  end

  service do
    run [opt_bin/"dotagent", "daemon"]
    keep_alive true
    log_path var/"log/dotagent.log"
    error_log_path var/"log/dotagent-error.log"
  end

  def caveats
    <<~EOS
      You're on the beta channel — builds track `main` and may break.
      For stable, use: brew install dotagent

      `dotagent@beta` is keg-only and isn't on $PATH by default. To
      use the beta binary directly:

        brew link --overwrite dotagent@beta

      Or invoke it by full path:

        #{opt_bin}/dotagent --version

      Quickstart (after linking):
        mkdir -p ~/.config/dotagent/agents
        dotagent doctor
        brew services start dotagent@beta
    EOS
  end

  test do
    assert_match "polyglot agent orchestrator", shell_output("#{bin}/dotagent --help").downcase
  end
end
