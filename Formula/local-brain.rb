# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.3"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.3/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "ed23d91eaa3c60e9cbd6b762506d328d1752c5f7099790ce3aba3b14407b81df"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.3/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "b213cc90ae626bc4d915365f119d8fa5fde2fb82af8e486b77331a9743b812f2"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.3/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9a17ce366281598aa5fa88cf6dac326068d90a6d68ae639ee48544927259af3c"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.3/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "8990d77ab19e7889487a2952c50ef79bb447523844819556df6ebb8dd93d61b9"
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
