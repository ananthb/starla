cask "starla-tray" do
  version :latest

  url "https://github.com/ananthb/starla/releases/latest/download/starla-macos-arm64.dmg"
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
