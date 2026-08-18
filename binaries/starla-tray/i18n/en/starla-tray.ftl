# Starla tray app — English source strings.
#
# This is the source catalogue: edit it here, and translations follow
# through Weblate (https://hosted.weblate.org/projects/starla/).
# Message identifiers are referenced from Rust with fl!(), so renaming
# one is a compile error until the call site is updated.

## Status shown at the top of the menu

status-connected = Connected
status-disconnected = Disconnected
status-not-registered = Not registered

# Shown under "Not registered" — the probe has no ID yet.
status-register-hint = Register the public key at atlas.ripe.net/apply/swprobe

# Fallback detail line when the probe is down for an unreported reason.
status-controller-unreachable = controller unreachable

# Shown when the tray cannot read the probe's status socket at all.
status-not-responding = Probe not responding

status-paused-indefinitely = Paused indefinitely

# $time is a clock time already formatted for the current locale, e.g. "14:05".
status-paused-until = Paused until { $time }

## Menu entries

# $id is the RIPE Atlas probe ID.
menu-probe-id = Probe { $id }

# $uptime comes from one of the uptime-* messages below.
menu-uptime = Uptime: { $uptime }

# $count is the total number of measurements run since the probe started.
menu-measurements = Measurements: { $count }

# One line per measurement type. $name is a RIPE Atlas measurement type
# ("ping", "traceroute", "dns", …) and is not translated.
menu-measurement-type = { $name }: { $count }

menu-resume = Resume measurements
menu-pause = Pause measurements
menu-start-probe = Start probe
menu-restart-probe = Restart probe
menu-stop-probe = Stop probe
menu-copy-key = Copy Public Key
menu-open-atlas = Open RIPE Atlas
menu-quit = Quit

## Pause durations, listed in the "Pause measurements" submenu

pause-30m = 30 minutes
pause-1h = 1 hour
pause-4h = 4 hours
pause-8h = 8 hours
pause-24h = 24 hours
pause-indefinite = Indefinitely

## Uptime, formatted at the coarsest unit that applies

uptime-days = { $days }d { $hours }h { $minutes }m
uptime-hours = { $hours }h { $minutes }m
uptime-minutes = { $minutes }m

## Tooltip shown when hovering the tray icon

# $id is the probe ID, $status one of the status-* messages.
tooltip-probe = Starla { $id }: { $status }
tooltip = Starla: { $status }
tooltip-not-responding = Starla: probe not responding

## Errors printed to the terminal when a daemon command fails

# $error is the underlying system error, which stays in English.
error-start-probe = Failed to start probe: { $error }
error-restart-probe = Failed to restart probe: { $error }
error-stop-probe = Failed to stop probe: { $error }

## Application metadata
##
## These generate the localized fields of the XDG desktop entry
## (packaging/starla-tray.desktop). "Starla" is a product name: leave it
## as-is unless your script needs transliteration.

desktop-name = Starla
desktop-comment = RIPE Atlas probe status
desktop-keywords = ripe;atlas;probe;network;measurement;
