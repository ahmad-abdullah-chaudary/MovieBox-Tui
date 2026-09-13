class MovieboxTui < Formula
  VERSION = "0.1.18"
  MACOS_SHA256 = "d58f233bb7f0255321d0b1d2b06cc47307fdb41209a46ebc8a2ea6bbd5d1ea3a"
  LINUX_X64_SHA256 = "6f8e95640f46169e639953221e5e00970082fdde2beda25f0bb80de9fc7a5f19"
  LINUX_ARM64_SHA256 = "f347ad98e8007aa3a631282ff52844eea012ee632fae27c5f78b7dc2fc722c3e"

  desc "Stream movies, shows, anime, and live TV from your terminal"
  homepage "https://github.com/ahmad-abdullah-chaudary/MovieBox-Tui"
  version VERSION
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    url "https://github.com/ahmad-abdullah-chaudary/MovieBox-Tui/releases/download/v#{VERSION}/MovieBox_macOS_Universal.tar.gz"
    sha256 MACOS_SHA256
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/ahmad-abdullah-chaudary/MovieBox-Tui/releases/download/v#{VERSION}/MovieBox_Linux_arm64.tar.gz"
      sha256 LINUX_ARM64_SHA256
    else
      url "https://github.com/ahmad-abdullah-chaudary/MovieBox-Tui/releases/download/v#{VERSION}/MovieBox_Linux_x64.tar.gz"
      sha256 LINUX_X64_SHA256
    end
  end

  def install
    bin.install "moviebox-tui"
  end

  test do
    system "#{bin}/moviebox-tui", "--version"
  end
end
