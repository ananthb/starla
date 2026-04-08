cask "starla-tray" do
  version :latest
  arch arm: "arm64", intel: "amd64"

  url "https://github.com/ananthb/starla/releases/latest/download/Starla-Tray-macos-#{arch}.dmg"
  name "Starla Tray"
  desc "System tray app for RIPE Atlas probe monitoring"
  homepage "https://github.com/ananthb/starla"

  app "Starla Tray.app"

  caveats <<~EOS
    Starla Tray shows the status of your RIPE Atlas probe.
    Make sure the starla probe is running:
      brew services start starla
  EOS
end
