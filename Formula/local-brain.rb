# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.5"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.5/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "2aa1cedb27e3339f0e041418582ad79e5e55602858a7fc0dcd84312574cc2d1b"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.5/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "a60e7e2221b933a7bc4787e322db2a4e11945ab0c14b18671d58edc2ebf234d4"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.5/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "4c1cefdf67478a89b4aec7f4ca8da5aef742ab26e59675460a8eff50847e8e48"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.5/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "aae9092822cb9c004ac8ec2a5430929711d24469efbc8127a9f1717eabca9a2d"
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
