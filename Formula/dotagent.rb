# dotagent — Homebrew formula.
#
# Lives at the repo root under Formula/ so this repo doubles as its own
# Homebrew tap:
#
#     brew tap avelino/dotagent https://github.com/avelino/dotagent
#     brew install dotagent
#     brew services start dotagent
#
# The release workflow (.github/workflows/release.yml) rewrites the
# `version` and the four `sha256` lines automatically after each
# tagged release — search for the `sha256 ""` placeholders and the
# `# DOTAGENT_*_SHA` markers below.
#
# The release archive `dotagent-<version>-<arch>-<os>.tar.gz` lays out:
#
#     bin/dotagent
#     bin/dotagent-plugin-preflight-warp
#     bin/dotagent-plugin-preflight-cmd
#     bin/dotagent-plugin-sink-roam
#     bin/dotagent-plugin-sink-file
#     LICENSE
#     README.md
#
# `bin.install Dir["bin/*"]` drops every binary in Homebrew's prefix so
# `dotagent` and every first-party plugin live next to each other on
# `$PATH` with zero extra config.

class Dotagent < Formula
  desc "Polyglot agent orchestrator with OS-native scheduling"
  homepage "https://github.com/avelino/dotagent"
  version "0.0.1" # DOTAGENT_VERSION
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/avelino/dotagent/releases/download/v#{version}/dotagent-#{version}-aarch64-darwin.tar.gz"
      sha256 "" # DOTAGENT_AARCH64_DARWIN_SHA
    end
    on_intel do
      url "https://github.com/avelino/dotagent/releases/download/v#{version}/dotagent-#{version}-x86_64-darwin.tar.gz"
      sha256 "" # DOTAGENT_X86_64_DARWIN_SHA
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/avelino/dotagent/releases/download/v#{version}/dotagent-#{version}-aarch64-linux.tar.gz"
      sha256 "" # DOTAGENT_AARCH64_LINUX_SHA
    end
    on_intel do
      url "https://github.com/avelino/dotagent/releases/download/v#{version}/dotagent-#{version}-x86_64-linux.tar.gz"
      sha256 "" # DOTAGENT_X86_64_LINUX_SHA
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
      Quickstart:
        mkdir -p ~/.config/dotagent/agents
        # drop your <name>/agent.toml in there
        dotagent doctor
        brew services start dotagent   # runs `dotagent daemon` via launchd/systemd

      Manual install of the launchd plist (optional, brew services does it for you):
        dotagent install
    EOS
  end

  test do
    assert_match "polyglot agent orchestrator", shell_output("#{bin}/dotagent --help").downcase
  end
end
