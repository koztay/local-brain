# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.10"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.10/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "bced795d36fe37b55cfeb4d2f26a17b8bb280c9eca4c196a0b6858755c4e515d"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.10/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "af4b95522cbb5b8120d8859ab911636f1a169d588cedceb47748e105f3cf9484"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.10/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "b6eb60a61b461c67cde10b08b5df8c703032fad740784d0c5562a252dd7ead48"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.10/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0bbed2ce2c9a2bb4ff29a17fe3bdb31c45f646445d8f99ec021c81a5256c88f9"
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
