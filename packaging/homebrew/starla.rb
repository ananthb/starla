class Starla < Formula
  desc "RIPE Atlas software probe"
  homepage "https://github.com/ananthb/starla"
  license "AGPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/ananthb/starla/releases/latest/download/starla-macos-arm64.dmg"
    end
  end

  def install
    bin.install "starla"
    etc.install "config.toml.example" => "starla/config.toml.example"
  end

  service do
    run [opt_bin/"starla"]
    keep_alive true
    log_path var/"log/starla.log"
    error_log_path var/"log/starla.log"
  end

  def caveats
    <<~EOS
      To start starla as a service:
        brew services start starla

      Register your probe at:
        https://atlas.ripe.net/apply/swprobe/

      Your public key is at:
        ~/.local/state/starla/probe_key.pub
    EOS
  end
end
