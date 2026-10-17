# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "3ee37c071b27f95e31edc5b72d37dd20fd515a27e2d0c87c4027cba878789880"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "a5308511e6235847aa598ae527a136e439ce63f388117e9d4fe31e63b1fc4792"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "7293450199fff425f48864d8c6eae613371613f3f8269224ab93ee8023bcb5c3"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "423b1f387bdf7830810454313f92379504ccb7746465e387d49d42a37d1343c1"
    end
  end

  head do
    url "https://github.com/koztay/local-brain.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args
    else
      bin.install "local-brain"
    end
  end

  def caveats
    <<~EOS
      Optional: install a local model runtime such as Ollama (brew install ollama) for
      summaries, local image reading and smarter compression.
      Then in a project: local-brain init && local-brain learn
    EOS
  end

  test do
    assert_match "local-brain", shell_output("#{bin}/local-brain --version")
  end
end
