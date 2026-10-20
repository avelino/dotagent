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
  version "0.6.2-beta.79" # DOTAGENT_BETA_VERSION
  license "MIT"

  keg_only :versioned_formula

  on_macos do
    on_arm do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-aarch64-darwin.tar.gz"
      sha256 "ae6fe7cb7b7e93ccea7fecc3446094cb079cab3faaed101a6104ed59fb51abe5" # DOTAGENT_BETA_AARCH64_DARWIN_SHA
    end
    on_intel do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-x86_64-darwin.tar.gz"
      sha256 "35553037f69be00597de996f355f96722f1a91b716116c841675e2b9c52bfc3c" # DOTAGENT_BETA_X86_64_DARWIN_SHA
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-aarch64-linux.tar.gz"
      sha256 "7543c93c0265db93622e64c51828a4a0f2224e0d72a29dddbf89cd8579f10ed3" # DOTAGENT_BETA_AARCH64_LINUX_SHA
    end
    on_intel do
      url "https://github.com/avelino/dotagent/releases/download/beta/dotagent-beta-x86_64-linux.tar.gz"
      sha256 "499b458d5182dbfc9e52703ed93b73be008c2ea8bd72a73c935b15cf46a0092a" # DOTAGENT_BETA_X86_64_LINUX_SHA
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
