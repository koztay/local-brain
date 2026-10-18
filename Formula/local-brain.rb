# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.8"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.8/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "c5fc3c8f994aa09fcd27eef37e8acf3094c2e8d72f97e9bd43e2927e92f0cebc"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.8/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "264575b10a641fa8116f05be6e5bc299d3d7436b267803a9ce50969bff17f6ba"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.8/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "8b9bfcc960ed71b3a51e48e546ea0cabc950464afcc23e88b53be9c46fd5e676"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.8/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "eb4b337663c32977fb08cd96a3d0d152af987161b07c567410d10bae3128c804"
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
      In a project: local-brain init && local-brain learn
      init offers to install and configure a local model (Ollama) for summaries,
      local image reading and smarter compression.
    EOS
  end

  test do
    assert_match "local-brain", shell_output("#{bin}/local-brain --version")
  end
end
