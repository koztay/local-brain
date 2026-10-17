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
      sha256 "8aebabd8a076ac8b33cd7a83d122c8a15439ec2a78c035763f0bac790b330fe8"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "6a3512b8946bc4a79563a3ae909e85d62f64f3a11cceb5db198f92edc5dad10a"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "011be4e25868827a65058eb44b361fe09a17c59edd7b04273fc57c9423f4de58"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.0/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "06064095835b69613ac363f2262c9dbe195c5f205aae1d22d857ab45d823723c"
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
