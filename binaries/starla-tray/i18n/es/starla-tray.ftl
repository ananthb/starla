## Status shown at the top of the menu

status-connected = Conectado
status-disconnected = Desconectado
status-not-registered = Sin registrar
status-register-hint = Registra la clave pública en atlas.ripe.net/apply/swprobe
status-controller-unreachable = no se puede contactar con el controlador
status-not-responding = La sonda no responde
status-paused-indefinitely = En pausa indefinidamente
status-paused-until = En pausa hasta las { $time }

## Menu entries

menu-probe-id = Sonda { $id }
menu-uptime = Tiempo activo: { $uptime }
menu-measurements = Mediciones: { $count }
menu-measurement-type = { $name }: { $count }
menu-resume = Reanudar mediciones
menu-pause = Pausar mediciones
menu-start-probe = Iniciar sonda
menu-restart-probe = Reiniciar sonda
menu-stop-probe = Detener sonda
menu-copy-key = Copiar clave pública
menu-open-atlas = Abrir RIPE Atlas
menu-quit = Salir

## Pause durations, listed in the "Pause measurements" submenu

pause-30m = 30 minutos
pause-1h = 1 hora
pause-4h = 4 horas
pause-8h = 8 horas
pause-24h = 24 horas
pause-indefinite = Indefinidamente

## Uptime, formatted at the coarsest unit that applies

uptime-days = { $days } d { $hours } h { $minutes } min
uptime-hours = { $hours } h { $minutes } min
uptime-minutes = { $minutes } min

## Tooltip shown when hovering the tray icon

tooltip-probe = Starla { $id }: { $status }
tooltip = Starla: { $status }
tooltip-not-responding = Starla: la sonda no responde

## Errors printed to the terminal when a daemon command fails

error-start-probe = No se pudo iniciar la sonda: { $error }
error-restart-probe = No se pudo reiniciar la sonda: { $error }
error-stop-probe = No se pudo detener la sonda: { $error }

## Application metadata

desktop-name = Starla
desktop-comment = Estado de la sonda RIPE Atlas
desktop-keywords = ripe;atlas;sonda;red;medición;
